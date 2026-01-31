// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, debug, warn, instrument};
use sentiric_sip_core::{
    SipPacket, Method, HeaderName, Header, 
    utils as sip_utils, 
    sdp::SdpBuilder,
    builder as sip_builder // Eklendi
};
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest, PlayAudioRequest};
use sentiric_contracts::sentiric::event::v1::{CallStartedEvent, CallEndedEvent, MediaInfo};
use tonic::Request;
use uuid::Uuid;
use prost_types::Timestamp;
use prost::Message;
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use std::net::{SocketAddr, IpAddr};
use std::time::SystemTime;

const DEFAULT_REMOTE_RTP_PORT: u16 = 10000;
const PROBE_SERVICE_NUMBER: &str = "9998";
const ECHO_TEST_NUMBER: &str = "9999";

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

    pub async fn handle_packet(&self, packet: SipPacket, src_addr: SocketAddr) {
        debug!("🔫 [SIP-IN] {} from {}", packet.method, src_addr);

        if packet.is_request {
            match packet.method {
                Method::Invite => self.handle_invite(packet, src_addr).await,
                Method::Ack => self.handle_ack(packet).await,
                Method::Bye => self.handle_bye(packet, src_addr).await,
                Method::Other(ref m) if m == "PUBLISH" || m == "MESSAGE" => {
                    self.handle_generic_success(packet, src_addr).await;
                }
                _ => { debug!("Method ignored: {:?}", packet.method); }
            }
        } else {
            self.handle_response(packet, src_addr).await;
        }
    }

    async fn handle_response(&self, mut resp: SipPacket, _src_addr: SocketAddr) {
        let call_id = resp.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        if let Some(session) = self.calls.get(&call_id) {
             if session.is_bridged {
                 if let Some(caller_addr) = session.caller_addr {
                     // B2BUA Response Proxying Logic
                     if !resp.headers.is_empty() && resp.headers[0].name == HeaderName::Via { 
                         resp.headers.remove(0); 
                     }
                     resp.headers.retain(|h| h.name != HeaderName::Contact);
                     resp.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
                     let _ = self.transport.send(&resp.to_bytes(), caller_addr).await;
                 }
             }
        }
    }
}

impl B2BuaEngine {
    #[instrument(skip(self, req), fields(call_id = %req.get_header_value(HeaderName::CallId).unwrap_or(&"unknown".to_string())))]
    async fn handle_invite(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();

        // Idempotency / Retransmission Check
        if let Some(session) = self.calls.get(&call_id) {
            if let Some(last_resp) = &session.last_invite_response {
                warn!(call_id, "🔄 Retransmission detected. Resending cached 200 OK.");
                let _ = self.transport.send(&last_resp.to_bytes(), src_addr).await;
                return;
            }
        }

        let trying = SipPacket::create_response_for(&req, 100, "Trying".to_string());
        let _ = self.transport.send(&trying.to_bytes(), src_addr).await;

        let local_tag = sip_utils::generate_tag("b2bua");
        
        let (remote_ip, remote_port) = self.extract_sdp_info(&req.body)
            .unwrap_or((src_addr.ip(), DEFAULT_REMOTE_RTP_PORT));
        let rtp_target_str = format!("{}:{}", remote_ip, remote_port);

        let to_user = sip_utils::extract_username_from_uri(&to);
        
        if self.is_special_service_number(&to_user) {
             info!("🤖 Special Service Number Detected: {}", to_user);
             self.start_ai_flow(call_id, from, to, local_tag, src_addr, &req, rtp_target_str).await;
             return;
        }

        let callee_contact = self.resolve_user_contact(&to_user).await;

        if let Some(callee_uri) = callee_contact {
             self.start_bridging_flow(call_id, from, to, local_tag, src_addr, &req, callee_uri).await;
        } else {
             self.start_ai_flow(call_id, from, to, local_tag, src_addr, &req, rtp_target_str).await;
        }
    }

    async fn handle_ack(&self, req: SipPacket) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            session.state = CallState::Established;
            debug!(call_id, "✅ ACK Received. Call Established.");
             if session.is_bridged {
                let target = if session.callee_addr.is_some() { session.callee_addr } else { session.caller_addr };
                if let Some(addr) = target {
                    info!("🔄 [BRIDGING] Relaying ACK to {}", addr);
                    let ack_b = req.clone(); 
                    let _ = self.transport.send(&ack_b.to_bytes(), addr).await;
                }
            }
        }
    }

    async fn handle_bye(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        
        let ok = SipPacket::create_response_for(&req, 200, "OK".to_string());
        let _ = self.transport.send(&ok.to_bytes(), src_addr).await;

        if let Some((_, session)) = self.calls.remove(&call_id) {
            if session.is_bridged {
                let target = if Some(src_addr) == session.caller_addr { session.callee_addr } else { session.caller_addr };
                if let Some(addr) = target {
                    info!("🔄 [BRIDGING] Relaying BYE to {}", addr);
                    let mut bye_b = req.clone();
                    // Via stripping for BYE
                    if !bye_b.headers.is_empty() && bye_b.headers[0].name == HeaderName::Via {
                        bye_b.headers.remove(0);
                    }
                    let _ = self.transport.send(&bye_b.to_bytes(), addr).await;
                }
            }
            if session.rtp_port > 0 { self.release_media_port(session.rtp_port).await; }
            self.publish_call_ended(&call_id).await;
        }
    }
    
    async fn handle_generic_success(&self, req: SipPacket, src_addr: SocketAddr) {
        let ok = SipPacket::create_response_for(&req, 200, "OK".to_string());
        let _ = self.transport.send(&ok.to_bytes(), src_addr).await;
    }
}

impl B2BuaEngine {
    async fn start_ai_flow(
        &self, 
        call_id: String, 
        from: String, 
        to: String, 
        local_tag: String, 
        src_addr: SocketAddr, 
        req: &SipPacket, 
        rtp_target_str: String
    ) {
        let rtp_port = match self.allocate_media_port(&call_id).await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "Media Port Allocation FAILED.");
                self.send_sip_error(req, 503, "Service Unavailable (Media)", src_addr).await;
                return;
            }
        };

        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Invited,
            from_uri: from.clone(),
            to_uri: to.clone(),
            rtp_port,
            local_tag: local_tag.clone(),
            caller_addr: Some(src_addr),
            callee_addr: None,
            is_bridged: false,
            last_invite_response: None,
        };

        // [REFACTOR] SdpBuilder
        let sdp_body = SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            .with_standard_codecs()
            .build();

        let mut ok_resp = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers_with_tag(&mut ok_resp, req, &local_tag);
        
        ok_resp.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
        ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        ok_resp.body = sdp_body.as_bytes().to_vec();

        let mut final_session = session;
        final_session.last_invite_response = Some(ok_resp.clone());
        self.calls.insert(call_id.clone(), final_session);

        if let Err(e) = self.transport.send(&ok_resp.to_bytes(), src_addr).await {
            error!(error = %e, "Failed to send 200 OK.");
            self.release_media_port(rtp_port).await;
            self.calls.remove(&call_id);
        } else {
            info!(call_id, rtp_port, "AI Flow Started.");
            self.publish_call_started(&call_id, rtp_port, &rtp_target_str, &from, &to).await;
            self.trigger_hole_punching(rtp_port, rtp_target_str).await;
        }
    }

    async fn start_bridging_flow(
        &self, 
        call_id: String, 
        from: String, 
        to: String, 
        local_tag: String, 
        src_addr: SocketAddr, 
        req: &SipPacket, 
        callee_uri: String
    ) {
        info!("🚀 [BRIDGING] Bridging {} -> {}", from, callee_uri);
             
        let callee_addr = sip_utils::extract_socket_addr(&callee_uri);

        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Invited,
            from_uri: from.clone(),
            to_uri: to.clone(),
            rtp_port: 0,
            local_tag: local_tag.clone(),
            caller_addr: Some(src_addr),
            callee_addr,
            is_bridged: true,
            last_invite_response: None,
        };
        self.calls.insert(call_id.clone(), session);

        let mut invite_b = req.clone();
        invite_b.uri = callee_uri.clone();
        
        let b2b_contact = sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port);
        
        invite_b.headers.retain(|h| h.name != HeaderName::Contact);
        invite_b.headers.push(b2b_contact);

        if let Some(target) = callee_addr {
            let _ = self.transport.send(&invite_b.to_bytes(), target).await;
            info!("📤 [BRIDGING] INVITE (Leg B) sent to {}", target);
        } else {
            error!("❌ [BRIDGING] Failed to parse callee address: {}", callee_uri);
            self.send_sip_error(req, 500, "Internal Error", src_addr).await;
        }
    }
}

impl B2BuaEngine {
    pub async fn initiate_call(&self, call_id: String, from: String, to: String) -> anyhow::Result<()> {
        info!("🚀 [OUTBOUND] Initiating call: {} -> {} (CallID: {})", from, to, call_id);
        
        let target_addr: SocketAddr = self.config.proxy_sip_addr.parse()
            .map_err(|e| anyhow::anyhow!("Invalid proxy address: {}", e))?;

        let mut invite = SipPacket::new_request(Method::Invite, to.clone());
        
        invite.headers.push(Header::new(HeaderName::From, format!("<{}>;tag={}", from, sip_utils::generate_tag("out"))));
        invite.headers.push(Header::new(HeaderName::To, format!("<{}>", to)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::MaxForwards, "70".to_string()));
        
        invite.headers.push(sip_builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
        
        let rtp_port = self.allocate_media_port(&call_id).await?;
        
        // [REFACTOR] SdpBuilder
        let sdp_body = SdpBuilder::new(self.config.public_ip.clone(), rtp_port as u16)
            .with_standard_codecs()
            .build();

        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        invite.body = sdp_body.as_bytes().to_vec();

        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Invited,
            from_uri: from,
            to_uri: to,
            rtp_port,
            local_tag: sip_utils::generate_tag("out"),
            caller_addr: None,
            callee_addr: Some(target_addr),
            is_bridged: false,
            last_invite_response: None,
        };
        self.calls.insert(call_id, session);

        self.transport.send(&invite.to_bytes(), target_addr).await
            .map_err(|e| anyhow::anyhow!("Failed to send INVITE: {}", e))
    }
}

impl B2BuaEngine {
    fn is_special_service_number(&self, user: &str) -> bool {
        user == PROBE_SERVICE_NUMBER || user == ECHO_TEST_NUMBER
    }

    async fn resolve_user_contact(&self, username: &str) -> Option<String> {
        let mut clients = self.clients.lock().await;
        
        let creds_req = Request::new(sentiric_contracts::sentiric::user::v1::GetSipCredentialsRequest {
            sip_username: username.to_string(),
            realm: self.config.sip_realm.clone(),
        });

        if clients.user.get_sip_credentials(creds_req).await.is_ok() {
             let lookup_req = Request::new(sentiric_contracts::sentiric::sip::v1::LookupContactRequest {
                 sip_uri: format!("sip:{}@{}", username, self.config.sip_realm), 
             });
             
             if let Ok(lookup_res) = clients.registrar.lookup_contact(lookup_req).await {
                 let uris = lookup_res.into_inner().contact_uris;
                 if !uris.is_empty() {
                     return Some(uris[0].clone());
                 }
             }
        }
        None
    }

    async fn allocate_media_port(&self, call_id: &str) -> anyhow::Result<u32> {
        let mut clients = self.clients.lock().await;
        let req = Request::new(AllocatePortRequest { call_id: call_id.to_string() });
        let resp = clients.media.allocate_port(req).await?.into_inner();
        Ok(resp.rtp_port)
    }

    async fn release_media_port(&self, port: u32) {
        let mut clients = self.clients.lock().await;
        let req = Request::new(ReleasePortRequest { rtp_port: port });
        let _ = clients.media.release_port(req).await;
    }

    async fn trigger_hole_punching(&self, rtp_port: u32, target_addr: String) {
        let mut clients = self.clients.lock().await;
        let req = Request::new(PlayAudioRequest {
            audio_uri: format!("file://audio/tr/system/nat_warmer.wav"),
            server_rtp_port: rtp_port,
            rtp_target_addr: target_addr,
        });
        
        if let Err(e) = clients.media.play_audio(req).await {
            warn!("⚠️ Hole punching trigger failed: {}", e);
        }
    }

    async fn publish_call_started(&self, call_id: &str, server_port: u32, caller_rtp: &str, from: &str, to: &str) {
        let event = CallStartedEvent {
            event_type: "call.started".to_string(),
            trace_id: Uuid::new_v4().to_string(),
            call_id: call_id.to_string(),
            from_uri: from.to_string(),
            to_uri: to.to_string(),
            timestamp: Some(Timestamp::from(SystemTime::now())),
            dialplan_resolution: None,
            media_info: Some(MediaInfo { caller_rtp_addr: caller_rtp.to_string(), server_rtp_port: server_port }),
        };
        let body = event.encode_to_vec();
        let _ = self.rabbitmq.publish_event_bytes("call.started", &body).await;
    }

    async fn publish_call_ended(&self, call_id: &str) {
        let event = CallEndedEvent {
            event_type: "call.ended".to_string(),
            trace_id: Uuid::new_v4().to_string(),
            call_id: call_id.to_string(),
            timestamp: Some(Timestamp::from(SystemTime::now())),
            reason: "normal_clearing".to_string(),
        };
        let body = event.encode_to_vec();
        let _ = self.rabbitmq.publish_event_bytes("call.ended", &body).await;
    }

    fn extract_sdp_info(&self, body: &[u8]) -> Option<(IpAddr, u16)> {
        let sdp_str = std::str::from_utf8(body).ok()?;
        let mut ip: Option<IpAddr> = None;
        let mut port: Option<u16> = None;
        for line in sdp_str.lines() {
            if line.starts_with("c=IN IP4") {
                if let Some(ip_str) = line.split_whitespace().last() {
                    if let Ok(parsed_ip) = ip_str.parse::<IpAddr>() { ip = Some(parsed_ip); }
                }
            }
            if line.starts_with("m=audio") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(p) = parts[1].parse::<u16>() { port = Some(p); }
                }
            }
        }
        if let (Some(i), Some(p)) = (ip, port) { Some((i, p)) } else { None }
    }

    async fn send_sip_error(&self, req: &SipPacket, code: u16, reason: &str, target: SocketAddr) {
        let resp = SipPacket::create_response_for(&req, code, reason.to_string());
        let _ = self.transport.send(&resp.to_bytes(), target).await;
    }

    fn copy_headers_with_tag(&self, resp: &mut SipPacket, req: &SipPacket, local_tag: &str) {
        for h in &req.headers {
            match h.name {
                HeaderName::Via | HeaderName::From | HeaderName::CallId | HeaderName::CSeq => {
                    resp.headers.push(h.clone());
                },
                HeaderName::To => {
                    let mut new_to = h.value.clone();
                    if !new_to.contains(";tag=") {
                        new_to.push_str(&format!(";tag={}", local_tag));
                    }
                    resp.headers.push(Header::new(HeaderName::To, new_to));
                }
                _ => {}
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric B2BUA".to_string()));
    }
}