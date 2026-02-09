// sentiric-b2bua-service/src/sip/handlers/calls.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use std::net::SocketAddr;
use std::str::FromStr;
use tracing::{info, error};
use sentiric_sip_core::{
    SipPacket, HeaderName, Header, SipUri,
    builder::SipResponseFactory,
    builder as sip_builder,
    transaction::SipTransaction,
};
use sentiric_contracts::sentiric::dialplan::v1::{ResolveDialplanRequest, ActionType};
use sentiric_contracts::sentiric::media::v1::PlayAudioRequest;
use sentiric_rtp_core::RtpEndpoint;
use tonic::Request;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
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

    pub async fn process_invite(&self, transport: Arc<sentiric_sip_core::SipTransport>, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        
        let to_uri = SipUri::from_str(&to).unwrap_or_else(|_| SipUri::from_str("sip:unknown@sentiric.local").unwrap());
        let to_aor = to_uri.user.unwrap_or_default();

        let trying = SipResponseFactory::create_100_trying(&req);
        let _ = transport.send(&trying.to_bytes(), src_addr).await;

        let dialplan_res = {
            let mut clients = self.clients.lock().await;
            clients.dialplan.resolve_dialplan(Request::new(ResolveDialplanRequest {
                caller_contact_value: from.clone(),
                destination_number: to_aor,
            })).await
        };

        match dialplan_res {
            Ok(response) => {
                let resolution = response.into_inner();
                let action = resolution.action.as_ref().unwrap();
                let action_type = ActionType::try_from(action.r#type).unwrap_or(ActionType::Unspecified);

                info!("🧠 [DIALPLAN] Decision: {:?} for Call {}", action_type, call_id);

                let rtp_port = match self.media_mgr.allocate_port(&call_id).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!("❌ Media failure: {}", e);
                        let _ = transport.send(&SipResponseFactory::create_error(&req, 503, "Media Error").to_bytes(), src_addr).await;
                        return;
                    }
                };
                
                // [CRITICAL FIX]: Hedefi `src_addr` (Proxy'nin IP'si) yerine, SDP'den gelen gerçek müşteri IP'si olarak al.
                // Bu, B2BUA'nın RabbitMQ'ya doğru bilgiyi basmasını sağlar.
                let client_media_port = self.media_mgr.extract_port_from_sdp(&req.body).unwrap_or(30000);
                let client_media_ip = SipUri::from_str(&from).map(|uri| uri.host).unwrap_or_else(|_| src_addr.ip().to_string());
                let client_rtp_target = format!("{}:{}", client_media_ip, client_media_port);

                if action_type == ActionType::EchoTest {
                    info!("🔊 [PBX-MODE] Activating Native Echo. Target: {}", self.config.sbc_sip_addr);
                    
                    let mut media_client = { self.clients.lock().await.media.clone() };
                    
                    // [CRITICAL FIX]: Warmer paketini SBC'ye gönder, Proxy'ye değil!
                    let hole_punch_target = self.config.sbc_sip_addr.clone();

                    info!("🔨 [HOLE-PUNCH] Sending warmer packet to SBC: {}", hole_punch_target);
                    let _ = media_client.play_audio(Request::new(PlayAudioRequest {
                        audio_uri: "file://audio/tr/system/nat_warmer.wav".to_string(),
                        server_rtp_port: rtp_port,
                        rtp_target_addr: hole_punch_target.clone(),
                    })).await;

                    // Echo modunu başlatırken de hedef olarak SBC'yi göster.
                    let _ = media_client.play_audio(Request::new(PlayAudioRequest {
                        audio_uri: "control://enable_echo".to_string(),
                        server_rtp_port: rtp_port,
                        rtp_target_addr: hole_punch_target,
                    })).await;
                }

                // ... (200 OK gönderme ve Session oluşturma mantığı aynı) ...

                let local_tag = sentiric_sip_core::utils::generate_tag("b2bua");
                let sdp_body = self.media_mgr.generate_sdp(rtp_port);
                let mut ok_resp = SipResponseFactory::create_200_ok(&req);
                if let Some(to_h) = ok_resp.headers.iter_mut().find(|h| h.name == HeaderName::To) {
                    if !to_h.value.contains(";tag=") { to_h.value.push_str(&format!(";tag={}", local_tag)); }
                }
                ok_resp.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
                ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                ok_resp.body = sdp_body;

                let mut tx = SipTransaction::new(&req).unwrap();
                tx.update_with_response(&ok_resp);

                let session = CallSession {
                    call_id: call_id.clone(),
                    state: CallState::Established,
                    from_uri: from.clone(),
                    to_uri: to.clone(),
                    rtp_port,
                    local_tag,
                    endpoint: RtpEndpoint::new(None),
                    active_transaction: Some(tx),
                };
                self.calls.insert(call_id.clone(), session);

                if transport.send(&ok_resp.to_bytes(), src_addr).await.is_ok() {
                    if action_type != ActionType::EchoTest {
                        self.event_mgr.publish_call_started(&call_id, rtp_port, &client_rtp_target, &from, &to, Some(resolution)).await;
                    }
                }
            },
            Err(e) => {
                error!("❌ Dialplan failure: {}", e);
                let _ = transport.send(&SipResponseFactory::create_error(&req, 500, "Routing Error").to_bytes(), src_addr).await;
            }
        }
    }

    pub async fn process_outbound_invite(&self, transport: Arc<sentiric_sip_core::SipTransport>, call_id: &str, from_uri: &str, to_uri: &str) -> anyhow::Result<()> {
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

        let session = CallSession {
            call_id: call_id.to_string(),
            state: CallState::Trying,
            from_uri: from_uri.to_string(),
            to_uri: to_uri.to_string(),
            rtp_port,
            local_tag,
            endpoint: RtpEndpoint::new(None),
            active_transaction: None,
        };
        self.calls.insert(call_id.to_string(), session);

        let proxy_addr: SocketAddr = self.config.proxy_sip_addr.parse()?;
        transport.send(&invite.to_bytes(), proxy_addr).await?;
        Ok(())
    }

    pub async fn process_bye(&self, transport: Arc<sentiric_sip_core::SipTransport>, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let _ = transport.send(&SipResponseFactory::create_200_ok(&req).to_bytes(), src_addr).await;
        if let Some((_, session)) = self.calls.remove(&call_id) {
            self.media_mgr.release_port(session.rtp_port).await;
            self.event_mgr.publish_call_ended(&call_id).await;
        }
    }

    pub async fn process_ack(&self, call_id: &str) {
        if let Some(mut s) = self.calls.get_mut(call_id) { s.state = CallState::Established; }
    }
}