// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, debug, warn, instrument};
use sentiric_sip_core::{
    SipPacket, Method, HeaderName, Header, 
    utils as sip_utils, 
    builder as sip_builder,
    TransactionEngine, TransactionAction
};
use sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanRequest;
use tonic::Request;
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use std::net::{SocketAddr, IpAddr};

use crate::sip::handlers::media::MediaManager;
use crate::sip::handlers::events::EventManager;

pub struct B2BuaEngine {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<InternalClients>>,
    calls: CallStore,
    transport: Arc<sentiric_sip_core::SipTransport>,
    media_mgr: MediaManager,
    event_mgr: EventManager,
}

impl B2BuaEngine {
    pub fn new(
        config: Arc<AppConfig>,
        clients: Arc<Mutex<InternalClients>>,
        calls: CallStore,
        transport: Arc<sentiric_sip_core::SipTransport>,
        rabbitmq: Arc<RabbitMqClient>,
    ) -> Self {
        Self { 
            config: config.clone(), 
            clients: clients.clone(), 
            calls, 
            transport,
            media_mgr: MediaManager::new(clients, config),
            event_mgr: EventManager::new(rabbitmq),
        }
    }

    pub async fn send_outbound_invite(&self, call_id: &str, from_uri: &str, to_uri: &str) -> anyhow::Result<()> {
         let rtp_port = self.media_mgr.allocate_port(call_id).await?;
         let sdp_body = self.media_mgr.generate_sdp(rtp_port);

         let mut invite = SipPacket::new_request(Method::Invite, to_uri.to_string());
         let local_tag = sip_utils::generate_tag("b2bua-out");
         
         invite.headers.push(Header::new(HeaderName::From, format!("<{}>;tag={}", from_uri, local_tag)));
         invite.headers.push(Header::new(HeaderName::To, format!("<{}>", to_uri)));
         invite.headers.push(Header::new(HeaderName::CallId, call_id.to_string()));
         invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
         invite.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));
         invite.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
         invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
         invite.body = sdp_body;

         let session = CallSession {
            call_id: call_id.to_string(),
            state: CallState::Trying,
            from_uri: from_uri.to_string(),
            to_uri: to_uri.to_string(),
            rtp_port,
            local_tag: local_tag.clone(),
            remote_tag: None,
            caller_addr: None,
            callee_addr: None,
            is_bridged: false,
            peer_call_id: None,
            active_transaction: None, 
        };
        self.calls.insert(call_id.to_string(), session);

        let proxy_addr: SocketAddr = self.config.proxy_sip_addr.parse()?;
        if let Err(e) = self.transport.send(&invite.to_bytes(), proxy_addr).await {
            error!("❌ [OUTBOUND] Transport error: {}", e);
            self.media_mgr.release_port(rtp_port).await;
            self.calls.remove(call_id);
            return Err(anyhow::anyhow!("Transport error: {}", e));
        }
        Ok(())
    }

    pub async fn handle_packet(&self, packet: SipPacket, src_addr: SocketAddr) {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        debug!("📨 [B2BUA-IN] {} from {} (CallID: {})", packet.method, src_addr, call_id);

        let action = if let Some(session) = self.calls.get(&call_id) {
             TransactionEngine::check(&session.active_transaction, &packet)
        } else {
             TransactionAction::ForwardToApp
        };

        match action {
            TransactionAction::RetransmitResponse(cached_resp) => {
                info!("🔄 [RETRANSMIT] Eski yanıt önbellekten gönderiliyor -> {}", src_addr);
                let _ = self.transport.send(&cached_resp.to_bytes(), src_addr).await;
                return;
            },
            TransactionAction::Ignore => {
                debug!("💤 [IGNORE] Mükerrer veya geçersiz işlem.");
                return;
            },
            TransactionAction::ForwardToApp => {}
        }

        if packet.is_request {
            match packet.method {
                Method::Invite => self.handle_invite(packet, src_addr).await,
                Method::Ack => self.handle_ack(packet).await,
                Method::Bye => self.handle_bye(packet, src_addr).await,
                _ => { debug!("Yoksayılan Method: {:?}", packet.method); }
            }
        } else {
            self.handle_response(packet, src_addr).await;
        }
    }

    #[instrument(skip(self, req), fields(call_id = %req.get_header_value(HeaderName::CallId).unwrap_or(&"unknown".to_string())))]
    async fn handle_invite(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to_header = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let to_aor = sip_utils::extract_aor(&to_header);

        // 1. Transaction Başlat ve Hemen 100 Trying Gönder (CRITICAL FIX)
        let mut transaction = sentiric_sip_core::SipTransaction::new(&req);
        let trying = SipPacket::create_response_for(&req, 100, "Trying".to_string());
        
        if let Some(tx) = &mut transaction {
            tx.update_on_response(&trying);
        }
        
        // Asenkron olarak gönder, hata olursa logla ama süreci durdurma
        if let Err(e) = self.transport.send(&trying.to_bytes(), src_addr).await {
            error!("❌ [100 Trying] Gönderilemedi: {}", e);
        } else {
            debug!("📤 [100 Trying] Gönderildi.");
        }
        
        let local_tag = sip_utils::generate_tag("b2bua");

        let mut clients = self.clients.lock().await;
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: from.clone(),
            destination_number: to_aor.clone(),
        });

        match clients.dialplan.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                // ActionData içindeki action'ı veya ana Action stringini kullan
                let action = if let Some(da) = &resolution.action {
                    da.action.as_str()
                } else {
                    "UNKNOWN"
                };
                
                info!(action, "🗺️ Dialplan Kararı Alındı");

                let (remote_ip, remote_port) = self.extract_sdp_info(&req.body).unwrap_or((src_addr.ip(), 10000));
                let rtp_target_str = format!("{}:{}", remote_ip, remote_port);

                match action {
                    "BRIDGE_CALL" => {
                         warn!("BRIDGE_CALL B2BUA üzerinden henüz tam desteklenmiyor.");
                         self.send_sip_error(&req, 488, "Not Acceptable Here", src_addr).await;
                    },
                    "START_ECHO_TEST" => {
                        // Echo Test: Medya portu al, 200 OK dön ve sesi geri yansıt (Loopback)
                        info!("🔊 Echo Test Başlatılıyor...");
                        self.start_ai_flow(call_id, from, to_header, local_tag, src_addr, &req, rtp_target_str, Some(resolution), transaction).await;
                    },
                    _ => { 
                        // AI Conversation veya diğer durumlar
                        self.start_ai_flow(call_id, from, to_header, local_tag, src_addr, &req, rtp_target_str, Some(resolution), transaction).await;
                    }
                }
            },
            Err(e) => {
                error!(error = %e, "❌ Dialplan Hatası");
                self.send_sip_error(&req, 503, "Service Unavailable", src_addr).await;
            }
        }
    }

    async fn start_ai_flow(
        &self, 
        call_id: String, 
        from: String, 
        to: String, 
        local_tag: String, 
        src_addr: SocketAddr, 
        req: &SipPacket, 
        rtp_target_str: String,
        dialplan_res: Option<sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanResponse>,
        mut transaction: Option<sentiric_sip_core::SipTransaction>
    ) {
        // MediaManager kullanılıyor
        let rtp_port = match self.media_mgr.allocate_port(&call_id).await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "Media Port Allocation FAILED.");
                self.send_sip_error(req, 503, "Media Error", src_addr).await;
                return;
            }
        };

        // MediaManager üzerinden SDP
        let sdp_body = self.media_mgr.generate_sdp(rtp_port);

        let mut ok_resp = SipPacket::create_response_for(req, 200, "OK".to_string());
        
        if let Some(to_h) = ok_resp.headers.iter_mut().find(|h| h.name == HeaderName::To) {
             if !to_h.value.contains(";tag=") {
                 to_h.value.push_str(&format!(";tag={}", local_tag));
             }
        }
        ok_resp.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
        ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        ok_resp.body = sdp_body;

        if let Some(tx) = &mut transaction {
            tx.update_on_response(&ok_resp);
        }

        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Trying,
            from_uri: from.clone(),
            to_uri: to.clone(),
            rtp_port,
            local_tag: local_tag.clone(),
            remote_tag: None,
            caller_addr: Some(src_addr),
            callee_addr: None,
            is_bridged: false,
            peer_call_id: None,
            active_transaction: transaction,
        };

        self.calls.insert(call_id.clone(), session);

        if let Err(e) = self.transport.send(&ok_resp.to_bytes(), src_addr).await {
            error!(error = %e, "200 OK Gönderilemedi.");
            self.media_mgr.release_port(rtp_port).await;
            self.calls.remove(&call_id);
        } else {
            info!(call_id, rtp_port, "✅ AI Çağrısı Kabul Edildi (200 OK).");
            
            // EventManager ve MediaManager kullanılıyor
            // Echo Test veya AI fark etmez, ikisi de medya başlatmalı
            self.event_mgr.publish_call_started(&call_id, rtp_port, &rtp_target_str, &from, &to, dialplan_res).await;
            self.media_mgr.trigger_hole_punching(rtp_port, rtp_target_str).await;
        }
    }

    async fn handle_ack(&self, req: SipPacket) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            session.state = CallState::Established;
            info!(call_id, "✅ ACK Alındı. Bağlantı kuruldu.");
        }
    }

    async fn handle_bye(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let ok = SipPacket::create_response_for(&req, 200, "OK".to_string());
        let _ = self.transport.send(&ok.to_bytes(), src_addr).await;

        if let Some((_, session)) = self.calls.remove(&call_id) {
            info!(call_id, "🛑 BYE Alındı. Çağrı sonlandırılıyor.");
            if session.rtp_port > 0 { 
                self.media_mgr.release_port(session.rtp_port).await; 
            }
            self.event_mgr.publish_call_ended(&call_id).await;
        }
    }

    async fn handle_response(&self, res: SipPacket, _src_addr: SocketAddr) {
        let call_id = res.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let status_code = res.status_code;

        if let Some(mut session) = self.calls.get_mut(&call_id) {
            info!("📩 [OUTBOUND-RES] {} {} (CallID: {})", status_code, res.reason, call_id);

            if status_code >= 180 && status_code < 200 {
                session.state = CallState::Ringing;
            }
            else if status_code == 200 {
                session.state = CallState::Established;
                
                if let Some((remote_ip, remote_port)) = self.extract_sdp_info(&res.body) {
                    let rtp_target = format!("{}:{}", remote_ip, remote_port);
                    info!("🎯 [OUTBOUND] Hedef Medya Adresi: {}", rtp_target);
                    self.media_mgr.trigger_hole_punching(session.rtp_port, rtp_target.clone()).await;

                    self.event_mgr.publish_call_started(
                        &call_id, 
                        session.rtp_port, 
                        &rtp_target, 
                        &session.from_uri, 
                        &session.to_uri, 
                        None
                    ).await;
                } else {
                    warn!("⚠️ [OUTBOUND] 200 OK içinde SDP bulunamadı!");
                }

                let mut ack = SipPacket::new_request(Method::Ack, session.to_uri.clone());
                ack.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
                ack.headers.push(Header::new(HeaderName::To, res.get_header_value(HeaderName::To).unwrap().clone()));
                ack.headers.push(Header::new(HeaderName::From, res.get_header_value(HeaderName::From).unwrap().clone()));
                ack.headers.push(Header::new(HeaderName::CSeq, "1 ACK".to_string()));
                ack.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));

                if let Ok(proxy_addr) = self.config.proxy_sip_addr.parse() {
                     let _ = self.transport.send(&ack.to_bytes(), proxy_addr).await;
                     info!("📤 [OUTBOUND] ACK gönderildi.");
                }
            }
            else if status_code >= 400 {
                warn!("❌ [OUTBOUND] Çağrı reddedildi/hata: {}", status_code);
                self.media_mgr.release_port(session.rtp_port).await;
            }
        }
    }

    fn extract_sdp_info(&self, body: &[u8]) -> Option<(IpAddr, u16)> {
        let sdp_str = std::str::from_utf8(body).ok()?;
        let mut ip: Option<IpAddr> = None;
        let mut port: Option<u16> = None;
        for line in sdp_str.lines() {
            if line.starts_with("c=IN IP4") {
                if let Some(ip_str) = line.split_whitespace().last() { ip = ip_str.parse().ok(); }
            }
            if line.starts_with("m=audio") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 { port = parts[1].parse().ok(); }
            }
        }
        if let (Some(i), Some(p)) = (ip, port) { Some((i, p)) } else { None }
    }

    async fn send_sip_error(&self, req: &SipPacket, code: u16, reason: &str, target: SocketAddr) {
        let resp = SipPacket::create_response_for(req, code, reason.to_string());
        let _ = self.transport.send(&resp.to_bytes(), target).await;
    }
}