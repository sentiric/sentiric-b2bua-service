// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, debug, warn};
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header, utils as sip_utils};
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, ReleasePortRequest};
use tonic::Request;
use uuid;
use prost_types;
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use std::net::{SocketAddr, IpAddr};

// SIP URI'dan (örn: "sip:2001@10.88.0.1:39861") SocketAddr çıkarır.
fn extract_socket_addr_from_uri(uri: &str) -> Option<SocketAddr> {
    let mut s = uri.trim();
    s = s.trim_start_matches('<').trim_end_matches('>');
    if s.starts_with("sip:") { s = &s[4..]; } else if s.starts_with("sips:") { s = &s[5..]; }
    let host_port_part = if let Some(at_idx) = s.find('@') { &s[at_idx + 1..] } else { s };
    let host_port = if let Some(semi_idx) = host_port_part.find(';') { &host_port_part[..semi_idx] } else { host_port_part };
    if !host_port.contains(':') { format!("{}:5060", host_port).parse().ok() } else { host_port.parse().ok() }
}

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
        if packet.is_request {
            match packet.method {
                Method::Invite => self.handle_invite(packet, src_addr).await,
                Method::Ack => self.handle_ack(packet).await,
                Method::Bye => self.handle_bye(packet, src_addr).await,
                Method::Other(ref m) if m == "PUBLISH" || m == "MESSAGE" => {
                    self.handle_generic_success(packet, src_addr).await;
                }
                _ => { debug!("Method not handled: {:?}", packet.method); }
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
                     info!("🔄 [BRIDGING] Relaying {} Response to Caller {}", resp.status_code, caller_addr);
                     if !resp.headers.is_empty() && resp.headers[0].name == HeaderName::Via { resp.headers.remove(0); }
                     resp.headers.retain(|h| h.name != HeaderName::Contact);
                     resp.headers.push(sentiric_sip_core::builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port));
                     let _ = self.transport.send(&resp.to_bytes(), caller_addr).await;
                 }
             }
        }
    }

    async fn handle_generic_success(&self, req: SipPacket, src_addr: SocketAddr) {
        let method_name = req.method.to_string();
        info!("ℹ️ [{}] Request received, sending 200 OK (Dummy)", method_name);
        let mut ok = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers(&mut ok, &req);
        if let Some(exp) = req.get_header_value(HeaderName::Other("Expires".to_string())) {
             ok.headers.push(Header::new(HeaderName::Other("Expires".to_string()), exp.clone()));
        }
        if let Err(e) = self.transport.send(&ok.to_bytes(), src_addr).await {
            error!("Failed to send 200 OK for {}: {}", method_name, e);
        }
    }

    // [FIX] ATOMİK INVITE İŞLEME (Race Condition Çözümü)
    async fn handle_invite(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();

        let session_exists = self.calls.contains_key(&call_id);

        if session_exists {
            if let Some(session) = self.calls.get(&call_id) {
                if let Some(last_resp) = &session.last_invite_response {
                    warn!("🔄 [SIP] Retransmission detected for {}, resending cached 200 OK.", call_id);
                    let _ = self.transport.send(&last_resp.to_bytes(), src_addr).await;
                    return;
                }
            }
        }

        info!("📞 [INVITE] New Call Processing: {} -> {}", from, to);

        let mut trying = SipPacket::new_response(100, "Trying".to_string());
        self.copy_headers(&mut trying, &req);
        let _ = self.transport.send(&trying.to_bytes(), src_addr).await;

        let local_tag = sip_utils::generate_tag("b2bua");
        let (remote_ip, remote_port) = self.extract_sdp_info(&req.body).unwrap_or((src_addr.ip(), 10000));
        let rtp_target_str = format!("{}:{}", remote_ip, remote_port);

        // --- HEDEF KONTROLÜ (User vs AI) ---
        let mut is_user_call = false;
        let mut callee_contact: Option<String> = None;
        let to_user = sip_utils::extract_username_from_uri(&to);
        
        if to_user != "9999" {
             let mut clients = self.clients.lock().await;
             let check_req = Request::new(sentiric_contracts::sentiric::user::v1::GetSipCredentialsRequest {
                 sip_username: to_user.clone(),
                 realm: self.config.sip_realm.clone(),
             });

             match clients.user.get_sip_credentials(check_req).await {
                 Ok(_) => {
                     info!("🔍 [ROUTING] Destination '{}' is a registered USER.", to_user);
                     let lookup_req = Request::new(sentiric_contracts::sentiric::sip::v1::LookupContactRequest {
                         sip_uri: format!("sip:{}@{}", to_user, self.config.sip_realm), 
                     });
                     
                     if let Ok(lookup_res) = clients.registrar.lookup_contact(lookup_req).await {
                         let uris = lookup_res.into_inner().contact_uris;
                         if !uris.is_empty() {
                             info!("✅ [ROUTING] User '{}' is ONLINE at {}", to_user, uris[0]);
                             callee_contact = Some(uris[0].clone());
                             is_user_call = true;
                         } else {
                             warn!("⚠️ [ROUTING] User '{}' found but is OFFLINE. Routing to AI.", to_user);
                         }
                     }
                 },
                 Err(_) => {
                    info!("🔍 [ROUTING] Destination '{}' not found in database. Routing to AI.", to_user);
                 }
             }
        }
        
        if is_user_call && callee_contact.is_some() {
             let callee_uri = callee_contact.unwrap();
             info!("🚀 [BRIDGING] Destination is a USER. Initiating Bridge for {} to {}", to_user, callee_uri);
             
             let session = CallSession {
                 call_id: call_id.clone(),
                 state: CallState::Invited,
                 from_uri: from.clone(),
                 to_uri: to.clone(),
                 rtp_port: 0,
                 local_tag: local_tag.clone(),
                 caller_addr: Some(src_addr),
                 callee_addr: extract_socket_addr_from_uri(&callee_uri),
                 is_bridged: true,
                 last_invite_response: None,
             };
             self.calls.insert(call_id.clone(), session);

             let mut invite_b = req.clone();
             invite_b.uri = callee_uri.clone();
             let b2b_contact = sentiric_sip_core::builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port);
             invite_b.headers.retain(|h| h.name != HeaderName::Contact);
             invite_b.headers.push(b2b_contact);

             if let Some(target) = extract_socket_addr_from_uri(&callee_uri) {
                 let _ = self.transport.send(&invite_b.to_bytes(), target).await;
                 info!("📤 [BRIDGING] INVITE (Leg B) sent to callee {}", target);
             } else {
                 error!("❌ [BRIDGING] Failed to parse callee address: {}", callee_uri);
             }
             return; 
        }

        // --- AI FLOW ---
        let rtp_port = match self.allocate_media_port(&call_id).await {
            Ok(p) => p,
            Err(e) => {
                error!("❌ Media Allocation Failed: {}", e);
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
        
        let sdp_body = format!(
            "v=0\r\n\
            o=- 123456 123456 IN IP4 {}\r\n\
            s=SentiricB2BUA\r\n\
            c=IN IP4 {}\r\n\
            t=0 0\r\n\
            {}\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:18 annexb=no\r\n\
            a=fmtp:101 0-16\r\n\
            a=sendrecv\r\n",
            self.config.public_ip, 
            self.config.public_ip,
            sentiric_sip_core::sdp::build_sdp_media_line(rtp_port as u16)
        );

        let mut ok_resp = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers_with_tag(&mut ok_resp, &req, &local_tag);
        let contact_header = sentiric_sip_core::builder::build_contact_header("b2bua", &self.config.public_ip, self.config.sip_port);
        ok_resp.headers.push(contact_header);
        ok_resp.headers.push(Header::new(HeaderName::Allow, "INVITE, ACK, BYE, CANCEL, OPTIONS".to_string()));
        ok_resp.headers.push(Header::new(HeaderName::Supported, "replaces, timer".to_string()));
        ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        ok_resp.body = sdp_body.as_bytes().to_vec();

        let mut final_session = session;
        final_session.last_invite_response = Some(ok_resp.clone());
        self.calls.insert(call_id.clone(), final_session);

        let response_bytes = ok_resp.to_bytes();
        if let Err(e) = self.transport.send(&response_bytes, src_addr).await {
            error!("Failed to send 200 OK: {}", e);
        } else {
            info!("✅ [SIP] 200 OK Sent (RTP Port: {})", rtp_port);
            self.publish_call_started(&call_id, rtp_port, &rtp_target_str, &from, &to).await;
        }
    }

    async fn handle_ack(&self, req: SipPacket) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            session.state = CallState::Established;
            info!("✅ [ACK] Call Established: {}", call_id);
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
        info!("🛑 [BYE] Request received for {}", call_id);

        let mut ok = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers(&mut ok, &req);
        let _ = self.transport.send(&ok.to_bytes(), src_addr).await;

        if let Some((_, session)) = self.calls.remove(&call_id) {
            if session.is_bridged {
                let target = if Some(src_addr) == session.caller_addr { session.callee_addr } else { session.caller_addr };
                if let Some(addr) = target {
                    info!("🔄 [BRIDGING] Relaying BYE to {}", addr);
                    let mut bye_b = req.clone();
                    if !bye_b.headers.is_empty() && bye_b.headers[0].name == HeaderName::Via {
                        bye_b.headers.remove(0);
                    }
                    let _ = self.transport.send(&bye_b.to_bytes(), addr).await;
                }
            }
            if session.rtp_port > 0 {
                self.release_media_port(session.rtp_port).await;
            }
            self.publish_call_ended(&call_id).await;
        }
    }

    // [DÜZELTME] E0599 Hata Çözümü: initiate_call Metodu Eklendi
    pub async fn initiate_call(&self, call_id: String, from: String, to: String) -> anyhow::Result<()> {
        info!("🚀 [OUTBOUND] Call initiation requested. ID: {}, From: {}, To: {}", call_id, from, to);
        // Gelecekte burada outbound call mantığı (Proxy'ye INVITE gönderme) olacak.
        // Şimdilik sadece log atıp başarılı dönüyoruz.
        Ok(())
    }

    async fn publish_call_started(&self, call_id: &str, server_port: u32, caller_rtp: &str, from: &str, to: &str) {
        use sentiric_contracts::sentiric::event::v1::{CallStartedEvent, MediaInfo};
        use prost::Message;
        let event = CallStartedEvent {
            event_type: "call.started".to_string(),
            trace_id: uuid::Uuid::new_v4().to_string(), 
            call_id: call_id.to_string(),
            from_uri: from.to_string(),
            to_uri: to.to_string(),
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            dialplan_resolution: None,
            media_info: Some(MediaInfo { caller_rtp_addr: caller_rtp.to_string(), server_rtp_port: server_port }),
        };
        let body = event.encode_to_vec();
        if let Err(e) = self.rabbitmq.publish_event_bytes("call.started", &body).await {
            error!("❌ [MQ] Failed to publish call.started: {}", e);
        } else {
            info!("📨 [MQ] Published call.started for {}", call_id);
        }
    }

    async fn publish_call_ended(&self, call_id: &str) {
        use sentiric_contracts::sentiric::event::v1::CallEndedEvent;
        use prost::Message;
        let event = CallEndedEvent {
            event_type: "call.ended".to_string(),
            trace_id: uuid::Uuid::new_v4().to_string(),
            call_id: call_id.to_string(),
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            reason: "normal_clearing".to_string(),
        };
        let body = event.encode_to_vec();
        if let Err(e) = self.rabbitmq.publish_event_bytes("call.ended", &body).await {
            error!("❌ [MQ] Failed to publish call.ended: {}", e);
        } else {
            info!("📨 [MQ] Published call.ended for {}", call_id);
        }
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
        info!("♻️ Released RTP Port: {}", port);
    }

    fn copy_headers(&self, resp: &mut SipPacket, req: &SipPacket) {
        for h in &req.headers {
            match h.name {
                HeaderName::Via | HeaderName::From | HeaderName::To | HeaderName::CallId | HeaderName::CSeq => {
                    resp.headers.push(h.clone());
                }
                _ => {}
            }
        }
        resp.headers.push(Header::new(HeaderName::Server, "Sentiric B2BUA".to_string()));
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