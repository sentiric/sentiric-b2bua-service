use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, debug, warn, instrument};
use sentiric_sip_core::{
    SipPacket, Method, HeaderName, Header, 
    utils as sip_utils, 
    sdp::SdpBuilder,
    builder as sip_builder
};
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest, PlayAudioRequest};
use sentiric_contracts::sentiric::event::v1::{CallStartedEvent, CallEndedEvent, MediaInfo};
use sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanRequest;
use tonic::Request;
use uuid::Uuid;
use prost_types::Timestamp;
use prost::Message; // Encode için
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use std::net::{SocketAddr, IpAddr};
use std::time::SystemTime;

const DEFAULT_REMOTE_RTP_PORT: u16 = 10000;

pub struct B2BuaEngine {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<InternalClients>>,
    calls: CallStore,
    transport: Arc<sentiric_sip_core::SipTransport>,
    rabbitmq: Arc<RabbitMqClient>,
}

impl B2BuaEngine {
    pub fn new(
        config: Arc<AppConfig>,
        clients: Arc<Mutex<InternalClients>>,
        calls: CallStore,
        transport: Arc<sentiric_sip_core::SipTransport>,
        rabbitmq: Arc<RabbitMqClient>,
    ) -> Self {
        Self { config, clients, calls, transport, rabbitmq }
    }

    /// [YENİ] Manuel Dış Arama Başlatıcı (UAC Modu)
    /// Bu metod, gRPC servisinden çağrılır ve operatöre INVITE gönderir.
    pub async fn send_outbound_invite(&self, call_id: &str, from_uri: &str, to_uri: &str) -> anyhow::Result<()> {
        info!("📞 [OUTBOUND] Yeni çağrı başlatılıyor. Call-ID: {}", call_id);

        // 1. Media Service'ten Port Kirala
        let rtp_port = self.allocate_media_port(call_id).await?;
        info!("✅ [OUTBOUND] Medya portu ayrıldı: {}", rtp_port);

        // 2. SDP Hazırla (Public IP ile)
        // Not: rtp_port u16'ya cast ediliyor.
        let sdp_body = SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            .with_standard_codecs()
            .build();

        // 3. SIP INVITE Paketi Oluştur
        // Hedef URI, config'den gelen Proxy adresine değil, 'to_uri'ye (gerçek hedefe) işaret etmeli
        // ancak transport katmanında fiziksel gönderim Proxy IP'sine yapılacak.
        let mut invite = SipPacket::new_request(Method::Invite, to_uri.to_string());
        
        let local_tag = sip_utils::generate_tag("b2bua-out");
        
        // Headerları Ekle
        invite.headers.push(Header::new(HeaderName::From, format!("<{}>;tag={}", from_uri, local_tag)));
        invite.headers.push(Header::new(HeaderName::To, format!("<{}>", to_uri)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.to_string()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));
        
        // Contact Header (B2BUA IP'si)
        invite.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
        
        // Content Type & Body
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.body = sdp_body.as_bytes().to_vec();

        // 4. Session State'i Oluştur ve Kaydet
        let session = CallSession {
            call_id: call_id.to_string(),
            state: CallState::Trying,
            from_uri: from_uri.to_string(),
            to_uri: to_uri.to_string(),
            rtp_port,
            local_tag: local_tag.clone(),
            remote_tag: None, // Karşı taraftan 200 OK gelince dolacak
            caller_addr: None, // Biz başlattığımız için caller_addr yok (veya kendimiziz)
            callee_addr: None, // Henüz bilmiyoruz (Proxy/SBC arkasında)
            is_bridged: false,
            peer_call_id: None,
            last_invite_request: Some(invite.clone()),
            last_response: None,
        };
        
        self.calls.insert(call_id.to_string(), session);

        // 5. Paketi Proxy Service'e Fırlat
        // Proxy adresi config'den okunur (host:port formatında olmalı)
        // Eğer parse hatası olursa, loglayıp hata dönüyoruz.
        let proxy_addr: SocketAddr = self.config.proxy_sip_addr.parse()
            .map_err(|e| anyhow::anyhow!("Proxy adresi parse edilemedi ({}): {}", self.config.proxy_sip_addr, e))?;

        if let Err(e) = self.transport.send(&invite.to_bytes(), proxy_addr).await {
            error!("❌ [OUTBOUND] INVITE gönderilemedi: {}", e);
            // Temizlik yap
            self.release_media_port(rtp_port).await;
            self.calls.remove(call_id);
            return Err(anyhow::anyhow!("Transport error: {}", e));
        }

        info!("📤 [OUTBOUND] SIP INVITE Proxy'ye ({}) iletildi.", proxy_addr);
        
        // 6. NAT Hole Punching (Opsiyonel ama iyi pratik)
        // Media service'in RTP dinlemeye başlaması için
        // self.trigger_hole_punching(rtp_port, "127.0.0.1:0".to_string()).await; 

        Ok(())
    }

    /// Ana Paket İşleyici
    pub async fn handle_packet(&self, packet: SipPacket, src_addr: SocketAddr) {
        debug!("📨 [B2BUA-IN] {} from {}", packet.method, src_addr);

        if packet.is_request {
            match packet.method {
                Method::Invite => self.handle_invite(packet, src_addr).await,
                Method::Ack => self.handle_ack(packet).await,
                Method::Bye => self.handle_bye(packet, src_addr).await,
                _ => { debug!("Yoksayılan Method: {:?}", packet.method); }
            }
        } else {
            // Yanıtları işle (Outbound çağrılar için kritik)
            self.handle_response(packet, src_addr).await;
        }
    }

    #[instrument(skip(self, req), fields(call_id = %req.get_header_value(HeaderName::CallId).unwrap_or(&"unknown".to_string())))]
    async fn handle_invite(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to_header = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let to_aor = sip_utils::extract_aor(&to_header);

        // 1. 100 Trying Gönder
        let trying = SipPacket::create_response_for(&req, 100, "Trying".to_string());
        let _ = self.transport.send(&trying.to_bytes(), src_addr).await;
        
        let local_tag = sip_utils::generate_tag("b2bua");

        // 2. Dialplan Sorgusu (Numara ne yapılacak?)
        let mut clients = self.clients.lock().await;
        let dialplan_req = Request::new(ResolveDialplanRequest {
            caller_contact_value: from.clone(),
            destination_number: to_aor.clone(),
        });

        match clients.dialplan.resolve_dialplan(dialplan_req).await {
            Ok(res) => {
                let resolution = res.into_inner();
                let action = resolution.action.as_ref().map_or("UNKNOWN", |a| a.action.as_str());
                
                info!(action, "🗺️ Dialplan Kararı Alındı");

                match action {
                    // Dahili Arama (Bridge)
                    "BRIDGE_CALL" => {
                         warn!("BRIDGE_CALL B2BUA üzerinden henüz tam desteklenmiyor, Proxy P2P yapmalıydı.");
                         self.send_sip_error(&req, 488, "Not Acceptable Here", src_addr).await;
                    },
                    // AI / IVR Senaryosu (Terminasyon)
                    _ => { 
                        // START_AI_CONVERSATION, PROCESS_GUEST_CALL vb.
                        let (remote_ip, remote_port) = self.extract_sdp_info(&req.body).unwrap_or((src_addr.ip(), 10000));
                        let rtp_target_str = format!("{}:{}", remote_ip, remote_port);
                        
                        // 3. AI Akışını Başlat
                        self.start_ai_flow(call_id, from, to_header, local_tag, src_addr, &req, rtp_target_str, Some(resolution)).await;
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
        dialplan_res: Option<sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanResponse>
    ) {
        // 1. Media Service'ten Port Kirala
        let rtp_port = match self.allocate_media_port(&call_id).await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "Media Port Allocation FAILED.");
                self.send_sip_error(req, 503, "Media Error", src_addr).await;
                return;
            }
        };

        // 2. Oturum Oluştur
        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Trying, // Henüz OK göndermedik
            from_uri: from.clone(),
            to_uri: to.clone(),
            rtp_port,
            local_tag: local_tag.clone(),
            remote_tag: None, // ACK gelince dolacak
            caller_addr: Some(src_addr),
            callee_addr: None,
            is_bridged: false,
            peer_call_id: None,
            last_invite_request: Some(req.clone()),
            last_response: None,
        };

        // 3. 200 OK Hazırla (SDP ile)
        let sdp_body = SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16).with_standard_codecs().build();
        let mut ok_resp = SipPacket::create_response_for(req, 200, "OK".to_string());
        
        // To header'ına Tag ekle
        if let Some(to_h) = ok_resp.headers.iter_mut().find(|h| h.name == HeaderName::To) {
             if !to_h.value.contains(";tag=") {
                 to_h.value.push_str(&format!(";tag={}", local_tag));
             }
        }
        
        ok_resp.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
        ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        ok_resp.body = sdp_body.as_bytes().to_vec();

        // 4. Oturumu Kaydet
        let mut final_session = session;
        final_session.last_response = Some(ok_resp.clone());
        self.calls.insert(call_id.clone(), final_session);

        // 5. Yanıtı Gönder
        if let Err(e) = self.transport.send(&ok_resp.to_bytes(), src_addr).await {
            error!(error = %e, "200 OK Gönderilemedi.");
            self.release_media_port(rtp_port).await;
            self.calls.remove(&call_id);
        } else {
            info!(call_id, rtp_port, "✅ AI Çağrısı Kabul Edildi (200 OK).");
            
            // 6. RabbitMQ'ya Event Bas (Agent Service Uyansın)
            self.publish_call_started(&call_id, rtp_port, &rtp_target_str, &from, &to, dialplan_res).await;
            
            // 7. NAT Hole Punching (Media Service'e "Ateş Et" de)
            self.trigger_hole_punching(rtp_port, rtp_target_str).await;
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
        
        // 200 OK Dön
        let ok = SipPacket::create_response_for(&req, 200, "OK".to_string());
        let _ = self.transport.send(&ok.to_bytes(), src_addr).await;

        if let Some((_, session)) = self.calls.remove(&call_id) {
            info!(call_id, "🛑 BYE Alındı. Çağrı sonlandırılıyor.");
            
            // Kaynakları Temizle
            if session.rtp_port > 0 { 
                self.release_media_port(session.rtp_port).await; 
            }
            // Event Bas
            self.publish_call_ended(&call_id).await;
        }
    }
    
    // [YENİ] Gelen Yanıtları (Response) İşleme
    async fn handle_response(&self, res: SipPacket, _src_addr: SocketAddr) {
        let call_id = res.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let status_code = res.status_code;

        // Session lookup
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            info!("📩 [OUTBOUND-RES] {} {} (CallID: {})", status_code, res.reason, call_id);

            // 1. Ringing (180/183)
            if status_code >= 180 && status_code < 200 {
                session.state = CallState::Ringing;
                // İleride buraya Ringing event'i eklenebilir.
            }
            // 2. Success (200 OK)
            else if status_code == 200 {
                session.state = CallState::Established;
                
                // Karşı tarafın SDP'sini parse et (Media Service'e hedef göstermek için)
                if let Some((remote_ip, remote_port)) = self.extract_sdp_info(&res.body) {
                    let rtp_target = format!("{}:{}", remote_ip, remote_port);
                    info!("🎯 [OUTBOUND] Hedef Medya Adresi: {}", rtp_target);
                    
                    // Media Service'e "Play" komutu göndererek NAT deliği aç ve akışı başlat
                    self.trigger_hole_punching(session.rtp_port, rtp_target.clone()).await;

                    // Agent Service'i bilgilendir (Call Started)
                    // Not: Outbound'da dialplan resolution boştur.
                    self.publish_call_started(
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

                // ACK Gönder (Zorunlu)
                // Yanıtın geldiği "To" başlığı (tag eklenmiş olabilir) kullanılmalı.
                let mut ack = SipPacket::new_request(Method::Ack, session.to_uri.clone()); // URI düzeltilmeli
                
                // Headerları kopyala/düzenle
                ack.headers.push(res.headers.iter().find(|h| h.name == HeaderName::From).unwrap().clone());
                ack.headers.push(res.headers.iter().find(|h| h.name == HeaderName::To).unwrap().clone());
                ack.headers.push(res.headers.iter().find(|h| h.name == HeaderName::CallId).unwrap().clone());
                
                let cseq_val = res.get_header_value(HeaderName::CSeq).unwrap_or(&"1 INVITE".to_string()).clone();
                let cseq_num = cseq_val.split_whitespace().next().unwrap_or("1");
                ack.headers.push(Header::new(HeaderName::CSeq, format!("{} ACK", cseq_num)));
                ack.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));

                // Proxy'ye ACK gönder
                if let Ok(proxy_addr) = self.config.proxy_sip_addr.parse() {
                     let _ = self.transport.send(&ack.to_bytes(), proxy_addr).await;
                     info!("📤 [OUTBOUND] ACK gönderildi.");
                }
            }
            // 3. Failure (4xx, 5xx, 6xx)
            else if status_code >= 400 {
                warn!("❌ [OUTBOUND] Çağrı reddedildi/hata: {}", status_code);
                // Kaynakları temizle
                self.release_media_port(session.rtp_port).await;
                // self.calls.remove(&call_id); // Burada remove yapamıyoruz çünkü deadlock riski var, drop ile halledilecek.
                // Event basılabilir (CallFailed).
            }
        }
    }

    // --- YARDIMCI FONKSİYONLAR ---

    async fn allocate_media_port(&self, call_id: &str) -> anyhow::Result<u32> {
        let mut clients = self.clients.lock().await;
        let resp = clients.media.allocate_port(Request::new(AllocatePortRequest { call_id: call_id.to_string() })).await?.into_inner();
        Ok(resp.rtp_port)
    }

    async fn release_media_port(&self, port: u32) {
        let mut clients = self.clients.lock().await;
        let _ = clients.media.release_port(Request::new(ReleasePortRequest { rtp_port: port })).await;
    }

    async fn trigger_hole_punching(&self, rtp_port: u32, target_addr: String) {
        let mut clients = self.clients.lock().await;
        // Boş bir ses dosyası çalarak Media Service'in hedef IP'ye paket atmasını sağlıyoruz.
        let _ = clients.media.play_audio(Request::new(PlayAudioRequest { 
            audio_uri: "file://audio/tr/system/nat_warmer.wav".to_string(), 
            server_rtp_port: rtp_port, 
            rtp_target_addr: target_addr 
        })).await;
    }

    async fn publish_call_started(
        &self, 
        call_id: &str, 
        server_port: u32, 
        caller_rtp: &str, 
        from: &str, 
        to: &str,
        dialplan_res: Option<sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanResponse>
    ) {
        let event = CallStartedEvent { 
            event_type: "call.started".to_string(), 
            trace_id: Uuid::new_v4().to_string(), 
            call_id: call_id.to_string(), 
            from_uri: from.to_string(), 
            to_uri: to.to_string(), 
            timestamp: Some(Timestamp::from(SystemTime::now())), 
            dialplan_resolution: dialplan_res, 
            media_info: Some(MediaInfo { 
                caller_rtp_addr: caller_rtp.to_string(), 
                server_rtp_port: server_port 
            }) 
        };
        let _ = self.rabbitmq.publish_event_bytes("call.started", &event.encode_to_vec()).await;
    }

    async fn publish_call_ended(&self, call_id: &str) {
        let event = CallEndedEvent { 
            event_type: "call.ended".to_string(), 
            trace_id: Uuid::new_v4().to_string(), 
            call_id: call_id.to_string(), 
            timestamp: Some(Timestamp::from(SystemTime::now())), 
            reason: "normal_clearing".to_string() 
        };
        let _ = self.rabbitmq.publish_event_bytes("call.ended", &event.encode_to_vec()).await;
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