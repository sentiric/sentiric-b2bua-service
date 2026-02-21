use std::sync::Arc;
use tokio::sync::Mutex;
use std::net::SocketAddr;
use std::str::FromStr;
use tracing::{info, error, warn, debug};
use sentiric_sip_core::{
    SipPacket, HeaderName, Header, SipUri,
    builder::SipResponseFactory,
    transaction::SipTransaction,
};
use sentiric_contracts::sentiric::dialplan::v1::{ResolveDialplanRequest, ActionType};
use sentiric_contracts::sentiric::media::v1::PlayAudioRequest;
use tonic::Request;
use crate::config::AppConfig;
use crate::sip::store::{CallStore, CallSession, CallSessionData, CallState};
use crate::grpc::client::InternalClients;
use crate::sip::handlers::media::MediaManager;
use crate::sip::handlers::events::EventManager;

pub struct CallHandler {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<InternalClients>>,
    calls: CallStore,
    media_mgr: MediaManager,
    event_mgr: EventManager,
}

impl CallHandler {
    pub fn new(config: Arc<AppConfig>, clients: Arc<Mutex<InternalClients>>, calls: CallStore, media_mgr: MediaManager, event_mgr: EventManager) -> Self {
        Self { config, clients, calls, media_mgr, event_mgr }
    }

    fn extract_rtp_target_from_sdp(&self, body: &[u8]) -> Option<String> {
        let sdp_str = String::from_utf8_lossy(body);
        let mut ip = String::new();
        let mut port = 0u16;
        for line in sdp_str.lines() {
            if line.starts_with("c=IN IP4 ") { ip = line[9..].trim().to_string(); }
            else if line.starts_with("m=audio ") {
                if let Some(p_str) = line.split_whitespace().nth(1) { port = p_str.parse().unwrap_or(0); }
            }
        }
        if !ip.is_empty() && port > 0 { Some(format!("{}:{}", ip, port)) } else { None }
    }

    pub async fn process_invite(&self, transport: Arc<sentiric_sip_core::SipTransport>, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let to_uri = SipUri::from_str(&to).unwrap_or_else(|_| SipUri::from_str("sip:unknown@sentiric.local").unwrap());
        let to_aor = to_uri.user.unwrap_or_default();

        let _ = transport.send(&SipResponseFactory::create_100_trying(&req).to_bytes(), src_addr).await;

        let dialplan_res = {
            let mut clients = self.clients.lock().await;
            clients.dialplan.resolve_dialplan(Request::new(ResolveDialplanRequest {
                caller_contact_value: from.clone(), destination_number: to_aor,
            })).await
        };

        match dialplan_res {
            Ok(response) => {
                let resolution = response.into_inner();
                let action = resolution.action.as_ref().unwrap();
                let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);

                info!(
                    event = "DIALPLAN_DECISION",
                    trace_id = %call_id,
                    sip.call_id = %call_id,
                    action.type = ?action_type,
                    "🧠 Dialplan kararı uygulandı"
                );

                let rtp_port = match self.media_mgr.allocate_port(&call_id).await {
                    Ok(p) => {
                        info!(
                            event = "MEDIA_PORT_ALLOCATED",
                            trace_id = %call_id,
                            rtp.port = p,
                            "🎤 RTP Portu tahsis edildi"
                        );
                        p
                    },
                    Err(e) => {
                        error!(event="MEDIA_ALLOC_FAIL", trace_id=%call_id, error=%e, "Media failure");
                        let _ = transport.send(&SipResponseFactory::create_error(&req, 503, "Media Error").to_bytes(), src_addr).await;
                        return;
                    }
                };

                let sbc_rtp_target = self.extract_rtp_target_from_sdp(&req.body)
                    .unwrap_or_else(|| {
                        warn!(event="SDP_PARSE_FAIL", trace_id=%call_id, "SDP parsing failed. Using source IP fallback.");
                        format!("{}:{}", src_addr.ip(), 30000) 
                    });

                if action_type == ActionType::EchoTest {
                    info!(event="ECHO_TEST_START", trace_id=%call_id, target=%sbc_rtp_target, "🔊 Echo Test Başlatılıyor");
                    let mut media_client = { self.clients.lock().await.media.clone() };
                    let _ = media_client.play_audio(Request::new(PlayAudioRequest {
                        audio_uri: "file://audio/tr/system/nat_warmer.wav".to_string(),
                        server_rtp_port: rtp_port, rtp_target_addr: sbc_rtp_target.clone(),
                    })).await;
                    let _ = media_client.play_audio(Request::new(PlayAudioRequest {
                        audio_uri: "control://enable_echo".to_string(),
                        server_rtp_port: rtp_port, rtp_target_addr: sbc_rtp_target.clone(),
                    })).await;
                }

                let local_tag = sentiric_sip_core::utils::generate_tag("b2bua");
                let sdp_body = self.media_mgr.generate_sdp(rtp_port);
                let mut ok_resp = SipResponseFactory::create_200_ok(&req);
                
                if let Some(to_h) = ok_resp.headers.iter_mut().find(|h| h.name == HeaderName::To) {
                    if !to_h.value.contains(";tag=") { to_h.value.push_str(&format!(";tag={}", local_tag)); }
                }

                let contact_uri = format!("<sip:b2bua@{}:{}>", self.config.public_ip, self.config.public_sip_port);
                ok_resp.headers.push(Header::new(HeaderName::Contact, contact_uri));
                ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                ok_resp.body = sdp_body;

                let mut tx = SipTransaction::new(&req).unwrap();
                tx.update_with_response(&ok_resp);

                let session_data = CallSessionData {
                    call_id: call_id.clone(),
                    state: CallState::Established,
                    from_uri: from.clone(),
                    to_uri: to.clone(),
                    rtp_port,
                    local_tag,
                };
                let mut session = CallSession::new(session_data);
                session.active_transaction = Some(tx);

                self.calls.insert(session).await;

                if transport.send(&ok_resp.to_bytes(), src_addr).await.is_ok() {
                    self.event_mgr.publish_call_started(&call_id, rtp_port, &sbc_rtp_target, &from, &to, Some(resolution)).await;
                    info!(event="CALL_ESTABLISHED", trace_id=%call_id, "✅ Çağrı kuruldu (200 OK)");
                }
            },
            Err(e) => {
                error!(event="DIALPLAN_ERROR", trace_id=%call_id, error=%e, "Dialplan hatası");
                let _ = transport.send(&SipResponseFactory::create_error(&req, 500, "Routing Error").to_bytes(), src_addr).await;
            }
        }
    }
    
    // --- GÜNCELLENMİŞ EKSİKSİZ METODLAR ---

    pub async fn process_outbound_invite(&self, transport: Arc<sentiric_sip_core::SipTransport>, call_id: &str, from_uri: &str, to_uri: &str) -> anyhow::Result<()> {
        info!(
            event = "OUTBOUND_CALL_INIT",
            trace_id = %call_id,
            sip.call_id = %call_id,
            to.uri = %to_uri,
            "🚀 Dış arama (Outbound) başlatılıyor"
        );

        let rtp_port = self.media_mgr.allocate_port(call_id).await?;
        let sdp_body = self.media_mgr.generate_sdp(rtp_port);
        let mut invite = SipPacket::new_request(sentiric_sip_core::Method::Invite, to_uri.to_string());
        let local_tag = sentiric_sip_core::utils::generate_tag("b2bua-out");
        
        invite.headers.push(Header::new(HeaderName::From, format!("<{}>;tag={}", from_uri, local_tag)));
        invite.headers.push(Header::new(HeaderName::To, format!("<{}>", to_uri)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.to_string()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.body = sdp_body;

        let session_data = CallSessionData {
            call_id: call_id.to_string(),
            state: CallState::Trying,
            from_uri: from_uri.to_string(),
            to_uri: to_uri.to_string(),
            rtp_port,
            local_tag,
        };
        self.calls.insert(CallSession::new(session_data)).await;

        let proxy_addr: SocketAddr = self.config.proxy_sip_addr.parse()?;
        
        transport.send(&invite.to_bytes(), proxy_addr).await?;
        
        info!(
            event = "OUTBOUND_INVITE_SENT",
            trace_id = %call_id,
            sip.method = "INVITE",
            net.dst.addr = %proxy_addr,
            "📤 Dış arama için INVITE gönderildi"
        );
        
        Ok(())
    }

    pub async fn process_bye(&self, transport: Arc<sentiric_sip_core::SipTransport>, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        
        info!(
            event = "BYE_RECEIVED",
            trace_id = %call_id,
            sip.call_id = %call_id,
            "🛑 Çağrı sonlandırma isteği alındı"
        );

        let _ = transport.send(&SipResponseFactory::create_200_ok(&req).to_bytes(), src_addr).await;
        
        if let Some(session) = self.calls.remove(&call_id).await {
            self.media_mgr.release_port(session.data.rtp_port).await;
            self.event_mgr.publish_call_ended(&call_id).await;
            
            info!(
                event = "CALL_TERMINATED",
                trace_id = %call_id,
                sip.call_id = %call_id,
                reason = "BYE from UA",
                "✅ Çağrı temizlendi ve kaynaklar serbest bırakıldı"
            );
        } else {
            warn!(
                event = "CALL_NOT_FOUND",
                trace_id = %call_id,
                "⚠️ BYE alındı ama aktif oturum bulunamadı"
            );
        }
    }

    pub async fn process_ack(&self, call_id: &str) {
        // State update
        self.calls.update_state(call_id, CallState::Established).await;
        
        info!(
            event = "SIP_ACK_RECEIVED",
            trace_id = %call_id,
            sip.call_id = %call_id,
            "ACK alındı, diyalog tamamen kuruldu"
        );
    }
}