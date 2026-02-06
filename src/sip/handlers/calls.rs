// sentiric-b2bua-service/src/sip/handlers/calls.rs
use std::sync::Arc;
use tokio::sync::Mutex;
use std::net::SocketAddr;
use tracing::{info, error, debug};
use sentiric_sip_core::{SipPacket, HeaderName, builder as sip_builder, utils as sip_utils};
use sentiric_contracts::sentiric::dialplan::v1::ResolveDialplanRequest;
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
    pub fn new(
        config: Arc<AppConfig>,
        clients: Arc<Mutex<InternalClients>>,
        calls: CallStore,
        media_mgr: MediaManager,
        event_mgr: EventManager,
    ) -> Self {
        Self { config, clients, calls, media_mgr, event_mgr }
    }

    pub async fn process_invite(&self, transport: Arc<sentiric_sip_core::SipTransport>, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let to_aor = sip_utils::extract_aor(&to);

        // 1. Ağ el sıkışması
        let trying = SipPacket::create_response_for(&req, 100, "Trying".to_string());
        let _ = transport.send(&trying.to_bytes(), src_addr).await;

        // 2. Karar verici katman sorgusu (Kilit yönetimi ile)
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
                
                // 3. Medya kaynağı tahsisi
                let rtp_port = match self.media_mgr.allocate_port(&call_id).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Media allocation failure: {}", e);
                        self.send_error(transport, &req, 503, "Media Engine Unavailable", src_addr).await;
                        return;
                    }
                };

                let local_tag = sip_utils::generate_tag("sentiric-b2bua");
                let sdp_body = self.media_mgr.generate_sdp(rtp_port);

                // 4. Kabul yanıtı hazırlama
                let mut ok_resp = SipPacket::create_response_for(&req, 200, "OK".to_string());
                if let Some(to_h) = ok_resp.headers.iter_mut().find(|h| h.name == HeaderName::To) {
                    if !to_h.value.contains(";tag=") { to_h.value.push_str(&format!(";tag={}", local_tag)); }
                }
                ok_resp.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
                ok_resp.headers.push(sentiric_sip_core::Header::new(HeaderName::ContentType, "application/sdp".to_string()));
                ok_resp.body = sdp_body;

                // 5. Oturum kaydı
                let session = CallSession {
                    call_id: call_id.clone(),
                    state: CallState::Trying,
                    from_uri: from.clone(),
                    to_uri: to.clone(),
                    rtp_port,
                    local_tag,
                    remote_tag: None,
                    caller_addr: Some(src_addr),
                    callee_addr: None,
                    is_bridged: false,
                    active_transaction: Some(sentiric_sip_core::SipTransaction::new(&req).unwrap()),
                };
                self.calls.insert(call_id.clone(), session);

                // 6. Finalizasyon
                if transport.send(&ok_resp.to_bytes(), src_addr).await.is_ok() {
                    self.event_mgr.publish_call_started(&call_id, rtp_port, &src_addr.to_string(), &from, &to, Some(resolution)).await;
                }
            },
            Err(e) => {
                error!("Dialplan resolution error: {}", e);
                self.send_error(transport, &req, 500, "Routing Failure", src_addr).await;
            }
        }
    }

    pub async fn process_bye(&self, transport: Arc<sentiric_sip_core::SipTransport>, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let ok = SipPacket::create_response_for(&req, 200, "OK".to_string());
        let _ = transport.send(&ok.to_bytes(), src_addr).await;

        if let Some((_, session)) = self.calls.remove(&call_id) {
            self.media_mgr.release_port(session.rtp_port).await;
            self.event_mgr.publish_call_ended(&call_id).await;
            info!("Call context cleared: {}", call_id);
        }
    }

    pub async fn process_ack(&self, call_id: &str) {
        if let Some(mut s) = self.calls.get_mut(call_id) {
            s.state = CallState::Established;
            debug!("Call established for: {}", call_id);
        }
    }

    async fn send_error(&self, transport: Arc<sentiric_sip_core::SipTransport>, req: &SipPacket, code: u16, reason: &str, target: SocketAddr) {
        let resp = SipPacket::create_response_for(req, code, reason.to_string());
        let _ = transport.send(&resp.to_bytes(), target).await;
    }
}