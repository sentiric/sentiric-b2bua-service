// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, instrument, warn};
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, SipTransport, utils as sip_core_utils};
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use sentiric_contracts::sentiric::media::v1::AllocatePortRequest;
use sentiric_contracts::sentiric::sip::v1::LookupContactRequest;
use sentiric_contracts::sentiric::event::v1::{CallStartedEvent, MediaInfo};
use prost::Message;
use tonic::Request;
use tokio::net::lookup_host;

pub struct B2BuaEngine {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<InternalClients>>,
    calls: CallStore,
    transport: Arc<SipTransport>,
    rabbitmq: Arc<RabbitMqClient>,
}

impl B2BuaEngine {
    pub fn new(
        config: Arc<AppConfig>, 
        clients: Arc<Mutex<InternalClients>>, 
        calls: CallStore,
        transport: Arc<SipTransport>,
        rabbitmq: Arc<RabbitMqClient>,
    ) -> Self {
        Self { config, clients, calls, transport, rabbitmq }
    }

    pub async fn handle_packet(&self, packet: SipPacket, src_addr: std::net::SocketAddr) {
        if packet.is_request {
            match packet.method {
                Method::Invite => self.handle_incoming_invite(packet, src_addr).await,
                Method::Bye => self.handle_bye(&packet).await,
                Method::Ack => self.handle_ack(&packet).await, 
                _ => {}
            }
        } else {
            if let Some(call_id) = packet.get_header_value(HeaderName::CallId) {
                if let Some(mut session) = self.calls.get_mut(call_id) {
                    self.handle_response(&mut session, &packet).await;
                }
            }
        }
    }

    pub async fn initiate_call(&self, call_id: String, from_uri: String, to_uri: String) -> anyhow::Result<()> {
        info!("gRPC InitiateCall İsteği: ID={} From={} To={}", call_id, from_uri, to_uri);

        let contact_uri = {
            let mut clients = self.clients.lock().await;
            match clients.registrar.lookup_contact(Request::new(LookupContactRequest {
                sip_uri: to_uri.clone(),
            })).await {
                Ok(res) => {
                    let uris = res.into_inner().contact_uris;
                    if uris.is_empty() {
                        return Err(anyhow::anyhow!("Hedef kullanıcı kayıtlı değil: {}", to_uri));
                    }
                    uris[0].clone()
                },
                Err(e) => return Err(anyhow::anyhow!("Registrar hatası: {}", e)),
            }
        };

        let rtp_port = {
            let mut clients = self.clients.lock().await;
            match clients.media.allocate_port(Request::new(AllocatePortRequest {
                call_id: call_id.clone(),
            })).await {
                Ok(res) => res.into_inner().rtp_port,
                Err(e) => return Err(anyhow::anyhow!("Media port ayrılamadı: {}", e)),
            }
        };

        self.initiate_outbound_leg(call_id.clone(), from_uri, to_uri, contact_uri, rtp_port, "OUTBOUND_INIT".to_string()).await;
        Ok(())
    }

    #[instrument(skip(self, packet))]
    async fn handle_incoming_invite(&self, packet: SipPacket, src_addr: std::net::SocketAddr) {
        let call_id = packet.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();

        if let Some(session) = self.calls.get(&call_id) {
            warn!(%call_id, "INVITE retransmission algılandı.");
            if session.state == CallState::Established {
                 if let Some(sdp) = &session.last_sdp {
                     info!("Önceki 200 OK (SDP ile) tekrar gönderiliyor...");
                     self.send_response(&packet, 200, "OK", Some(sdp.clone()), src_addr).await;
                 }
            } else {
                 info!("180 Ringing (Retransmission) gönderiliyor...");
                 self.send_response(&packet, 180, "Ringing", None, src_addr).await;
            }
            return;
        }

        let from = packet.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = packet.get_header_value(HeaderName::To).cloned().unwrap_or_default();
        let target_aor = sip_core_utils::extract_aor(&to);
        let from_aor = sip_core_utils::extract_aor(&from);

        info!(%call_id, "B2BUA: Incoming Call: {} -> {}", from, target_aor);
        self.send_response(&packet, 100, "Trying", None, src_addr).await;

        let contact_uri = {
            let mut clients = self.clients.lock().await;
            match clients.registrar.lookup_contact(Request::new(LookupContactRequest {
                sip_uri: target_aor.clone(),
            })).await {
                Ok(res) => {
                    let uris = res.into_inner().contact_uris;
                    if !uris.is_empty() { Some(uris[0].clone()) } else { None }
                },
                Err(e) => { warn!("Registrar Lookup Hatası: {}", e); None }
            }
        };

        // [FIX] Suffix Kaldırıldı: Agent Service ile uyumluluk için saf call_id kullanılıyor.
        let port_a = {
            let mut clients = self.clients.lock().await;
            match clients.media.allocate_port(Request::new(AllocatePortRequest {
                call_id: call_id.clone(), // "_A" YOK!
            })).await {
                Ok(res) => res.into_inner().rtp_port,
                Err(e) => {
                    error!("Media AllocatePort Failed: {}", e);
                    self.send_response(&packet, 503, "Media Error", None, src_addr).await;
                    return;
                }
            }
        };

        if let Some(target_contact) = contact_uri {
            info!("Target is a User. Bridging to {}", target_contact);
            let session = CallSession {
                call_id: call_id.clone(),
                state: CallState::Trying,
                from_uri: from.clone(), to_uri: to.clone(),
                rtp_port: port_a, remote_tag: None,
                peer_call_id: Some(format!("{}_B_LEG", call_id)),
                source_addr: Some(src_addr),
                last_sdp: None,
            };
            self.calls.insert(call_id.clone(), session);
            self.send_response(&packet, 180, "Ringing", None, src_addr).await;
            let outbound_call_id = format!("{}_B_LEG", call_id);
            self.initiate_outbound_leg(outbound_call_id, from, target_aor, target_contact, port_a, call_id).await;
        } else {
            info!("Target NOT found in Registrar. Treating as SYSTEM/AI call.");
            let sdp = self.create_sdp(port_a);
            self.send_response(&packet, 200, "OK", Some(sdp.clone()), src_addr).await;
            
            let session = CallSession {
                call_id: call_id.clone(),
                state: CallState::Established,
                from_uri: from.clone(), to_uri: to.clone(),
                rtp_port: port_a, remote_tag: None,
                peer_call_id: None, 
                source_addr: Some(src_addr),
                last_sdp: Some(sdp),
            };
            self.calls.insert(call_id.clone(), session);

            let event = CallStartedEvent {
                event_type: "call.started".to_string(),
                trace_id: call_id.clone(), call_id: call_id.clone(),
                from_uri: from_aor, to_uri: target_aor,
                timestamp: None,
                media_info: Some(MediaInfo {
                    caller_rtp_addr: src_addr.ip().to_string(),
                    server_rtp_port: port_a,
                }),
                dialplan_resolution: None,
            };
            
            let payload = event.encode_to_vec();
            if let Err(e) = self.rabbitmq.publish_event_bytes("call.started", &payload).await {
                error!("Failed to publish PROTOBUF call.started event: {}", e);
            } else {
                info!("🚀 call.started PROTOBUF event published ({} bytes).", payload.len());
            }
        }
    }

    async fn handle_ack(&self, _packet: &SipPacket) {}
    async fn handle_bye(&self, _packet: &SipPacket) {}

    async fn initiate_outbound_leg(&self, call_id: String, from_header: String, to_aor: String, contact_uri: String, rtp_port: u32, original_call_id: String) {
         let sdp = self.create_sdp(rtp_port);
         let request_uri = if !contact_uri.starts_with("sip:") {
            format!("sip:{}", contact_uri)
        } else {
            contact_uri.clone()
        };
        
        let mut invite = SipPacket::new_request(Method::Invite, request_uri);
        let branch = sip_core_utils::generate_branch_id();
        
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch={}", self.config.public_ip, self.config.sip_port, branch)));
        invite.headers.push(Header::new(HeaderName::From, from_header.clone()));
        invite.headers.push(Header::new(HeaderName::To, format!("<sip:{}>", to_aor)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:b2bua@{}:{}>", self.config.public_ip, self.config.sip_port)));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric B2BUA".to_string()));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.body = sdp.as_bytes().to_vec();

        self.calls.insert(call_id.clone(), CallSession {
            call_id: call_id.clone(),
            state: CallState::Trying,
            from_uri: from_header,
            to_uri: to_aor,
            rtp_port,
            remote_tag: None,
            peer_call_id: Some(original_call_id),
            source_addr: None,
            last_sdp: Some(sdp),
        });

        let bytes = invite.to_bytes();
        match lookup_host(&self.config.proxy_sip_addr).await {
            Ok(mut addrs) => {
                if let Some(target_addr) = addrs.next() {
                    let _ = self.transport.send(&bytes, target_addr).await;
                    info!("Outbound INVITE sent (ID: {}) -> {}", call_id, target_addr);
                }
            },
            Err(e) => error!("Proxy DNS Error ({}): {}", self.config.proxy_sip_addr, e),
        }
    }

    async fn handle_response(&self, session: &mut CallSession, packet: &SipPacket) {
         if packet.status_code >= 200 && packet.status_code < 300 {
            info!("Outbound Leg Answered (2xx): {}", session.call_id);
            session.state = CallState::Established;

            if let Some(peer_id) = &session.peer_call_id {
                if let Some(mut peer_session) = self.calls.get_mut(peer_id) {
                    peer_session.state = CallState::Established;
                    if let Some(src_addr) = peer_session.source_addr {
                        let sdp = self.create_sdp(peer_session.rtp_port);
                        
                        peer_session.last_sdp = Some(sdp.clone());

                        self.send_response(packet, 200, "OK", Some(sdp), src_addr).await; 
                    }
                }
            }
        }
    }

    async fn send_response(&self, req: &SipPacket, code: u16, reason: &str, body: Option<String>, target: std::net::SocketAddr) {
        let mut resp = SipPacket::new_response(code, reason.to_string());
        for h in &req.headers {
            if let HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq = h.name {
                resp.headers.push(h.clone());
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric B2BUA".to_string()));
        
        if let Some(b) = body {
            resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
            resp.body = b.as_bytes().to_vec();
        } else {
            resp.headers.push(Header::new(HeaderName::ContentLength, "0".to_string()));
        }

        let _ = self.transport.send(&resp.to_bytes(), target).await;
    }

    fn create_sdp(&self, port: u32) -> String {
        format!(
            "v=0\r\n\
            o=- {0} {0} IN IP4 {1}\r\n\
            s=Sentiric_B2BUA\r\n\
            c=IN IP4 {1}\r\n\
            t=0 0\r\n\
            m=audio {2} RTP/AVP 0 8 18 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-16\r\n\
            a=sendrecv\r\n",
            rand::random::<u64>(),
            self.config.public_ip,
            port
        )
    }
}