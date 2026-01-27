// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, debug, warn};
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header, utils as sip_utils};
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, PlayAudioRequest, ReleasePortRequest};
use tonic::Request;
use uuid; // Eklendi (TODO: Cargo.toml check) (Self-correction: Will verify)
use prost_types;
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use std::net::{SocketAddr, IpAddr};


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
        Self {
            config,
            clients,
            calls,
            transport,
            rabbitmq,
        }
    }

    /// Gelen SIP paketlerini işler.
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
            // Yanıtları işle (B2B Köprüleme için)
            self.handle_response(packet, src_addr).await;
        }
    }

    /// Başka bir uç noktadan gelen yanıtları işler.
    async fn handle_response(&self, mut resp: SipPacket, _src_addr: SocketAddr) {
        let call_id = resp.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        
        if let Some(session) = self.calls.get(&call_id) {
             if session.is_bridged {
                 // BRIDGING: Response Relay (Leg B -> Leg A)
                 if let Some(caller_addr) = session.caller_addr {
                     info!("🔄 [BRIDGING] Relaying {} Response to Caller {}", resp.status_code, caller_addr);
                     
                     // Via header'ını temizle (SBC-B2BUA arasındakini)
                     if !resp.headers.is_empty() && resp.headers[0].name == HeaderName::Via {
                         resp.headers.remove(0);
                     }
                     
                     // Contact'ı güncelle
                     resp.headers.retain(|h| h.name != HeaderName::Contact);
                     resp.headers.push(sentiric_sip_core::builder::build_contact_header(
                         "b2bua", &self.config.public_ip, self.config.sip_port
                     ));

                     let _ = self.transport.send(&resp.to_bytes(), caller_addr).await;
                 }
             }
        }
    }

    /// Genel amaçlı 200 OK döner (PUBLISH, MESSAGE vb. için)
    async fn handle_generic_success(&self, req: SipPacket, src_addr: SocketAddr) {
        let method_name = req.method.to_string();
        info!("ℹ️ [{}] Request received, sending 200 OK (Dummy)", method_name);
        
        let mut ok = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers(&mut ok, &req);
        // Expires header'ı varsa ekle (özellikle REGISTER/PUBLISH için önemli olabilir ama şimdilik basit tutalım)
        if let Some(exp) = req.get_header_value(HeaderName::Other("Expires".to_string())) {
             ok.headers.push(Header::new(HeaderName::Other("Expires".to_string()), exp.clone()));
        }
        
        if let Err(e) = self.transport.send(&ok.to_bytes(), src_addr).await {
            error!("Failed to send 200 OK for {}: {}", method_name, e);
        }
    }

    async fn handle_invite(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();

        // 1. [KRİTİK] Retransmission (Tekrar) Kontrolü
        // Eğer bu Call-ID için zaten bir oturum ve üretilmiş bir cevap varsa,
        // yeni bir işlem yapma (port alma, tag üretme vb.).
        // Sadece hafızadaki cevabı aynen (idempotent) geri gönder.
        if let Some(session) = self.calls.get(&call_id) {
            if let Some(last_resp) = &session.last_invite_response {
                warn!("🔄 [SIP] Retransmission detected for {}, resending cached 200 OK.", call_id);
                let _ = self.transport.send(&last_resp.to_bytes(), src_addr).await;
                return;
            }
        }

        info!("📞 [INVITE] New Call: {} -> {}", from, to);

        // 100 Trying (Stateless, kaydetmeye gerek yok)
        let mut trying = SipPacket::new_response(100, "Trying".to_string());
        self.copy_headers(&mut trying, &req);
        let _ = self.transport.send(&trying.to_bytes(), src_addr).await;

        // Tag üretimi (Hem B2B hem AI için kullanılacak)
        let local_tag = sip_utils::generate_tag("b2bua");

        // 2. SDP Analizi
        let (remote_ip, remote_port) = self.extract_sdp_info(&req.body).unwrap_or((src_addr.ip(), 10000));
        let rtp_target_str = format!("{}:{}", remote_ip, remote_port);
        info!("🎯 Caller RTP Target Resolved: {}", rtp_target_str);

        // --- GÜNCELLEME: Hedef Kontrolü (User vs AI) ---
        let mut is_user_call = false;
        let mut callee_contact: Option<String> = None;
        let to_user = sip_utils::extract_username_from_uri(&to);
        
        // 9999 AI için rezerve, diğerleri User olabilir
        if to_user != "9999" {
             let mut clients = self.clients.lock().await;
             
             // 1. Kullanıcı var mı?
             let check_req = Request::new(sentiric_contracts::sentiric::user::v1::GetSipCredentialsRequest {
                 sip_username: to_user.clone(),
                 realm: "sip.azmisahin.com".to_string(), // TODO: Domain dynamic olmalı
             });

             match clients.user.get_sip_credentials(check_req).await {
                 Ok(_) => {
                     info!("🔍 [ROUTING] Destination '{}' is a registered USER.", to_user);
                     
                     // 2. Kullanıcı Online mı? (Registrar'dan adresini sor)
                     let lookup_req = Request::new(sentiric_contracts::sentiric::sip::v1::LookupContactRequest {
                         sip_uri: format!("sip:{}@sip.azmisahin.com", to_user),
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
                 Err(status) => {
                     if status.code() == tonic::Code::NotFound {
                        info!("🔍 [ROUTING] Destination '{}' not found in database. Routing to AI.", to_user);
                     } else {
                        warn!("⚠️ [ROUTING] User check failed for '{}': {}. Defaulting to AI.", to_user, status);
                     }
                 }
             }
        }
        
        if is_user_call && callee_contact.is_some() {
             let callee_uri = callee_contact.unwrap();
             info!("🚀 [BRIDGING] Destination is a USER. Initiating Bridge for {} to {}", to_user, callee_uri);
             
             // --- B2B BRIDGING LOGIC (ALPHA) ---
             // 1. Session'ı B2B olarak kaydet
             let session = CallSession {
                 call_id: call_id.clone(),
                 state: CallState::Invited,
                 from_uri: from.clone(),
                 to_uri: to.clone(),
                 rtp_port: 0, // Bridging için medya portu yönetimini sonra ekleyelim
                 local_tag: local_tag.clone(),
                 caller_addr: Some(src_addr),
                 callee_addr: callee_uri.parse::<SocketAddr>().ok(), // Basit parse
                 is_bridged: true,
                 last_invite_response: None,
             };
             self.calls.insert(call_id.clone(), session);

             // 2. [KRİTİK] Yeni INVITE (Leg B) oluştur ve Callee'ye gönder
             // Şimdilik gelen paketi clone edip hedefini değiştiriyoruz.
             let mut invite_b = req.clone();
             invite_b.uri = callee_uri.clone();
             
             // Contact header'ını B2BUA IP'si ile güncelle
             let b2b_contact = sentiric_sip_core::builder::build_contact_header(
                 "b2bua",
                 &self.config.public_ip,
                 self.config.sip_port
             );
             
             // Eskisini silip yenisini ekle
             invite_b.headers.retain(|h| h.name != HeaderName::Contact);
             invite_b.headers.push(b2b_contact);

             if let Ok(target) = callee_uri.parse::<SocketAddr>() {
                 if let Err(e) = self.transport.send(&invite_b.to_bytes(), target).await {
                     error!("❌ [BRIDGING] Failed to send INVITE to callee {}: {}", target, e);
                 } else {
                     info!("📤 [BRIDGING] INVITE (Leg B) sent to callee {}", target);
                 }
             } else {
                 error!("❌ [BRIDGING] Failed to parse callee address: {}", callee_uri);
             }

             return; 
        }

        // 3. Media Service'ten Port İste (AI Flow)
        let rtp_port = match self.allocate_media_port(&call_id).await {
            Ok(p) => p,
            Err(e) => {
                error!("❌ Media Allocation Failed: {}", e);
                let mut err_resp = SipPacket::new_response(503, "Service Unavailable".to_string());
                self.copy_headers(&mut err_resp, &req);
                let _ = self.transport.send(&err_resp.to_bytes(), src_addr).await;
                return;
            }
        };

        // 4. Session Oluştur (AI Flow)
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
        self.calls.insert(call_id.clone(), session);

        // 5. 200 OK (SDP) Hazırla
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
        
        let contact_header = sentiric_sip_core::builder::build_contact_header(
            "b2bua",
            &self.config.public_ip,
            self.config.sip_port
        );
        
        ok_resp.headers.push(contact_header);
        ok_resp.headers.push(Header::new(HeaderName::Allow, "INVITE, ACK, BYE, CANCEL, OPTIONS".to_string()));
        ok_resp.headers.push(Header::new(HeaderName::Supported, "replaces, timer".to_string()));
        ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        ok_resp.body = sdp_body.as_bytes().to_vec();

        // 6. Session'ı Güncelle (Response Cache)
        if let Some(mut sess) = self.calls.get_mut(&call_id) {
            sess.last_invite_response = Some(ok_resp.clone());
        }

        // 7. Yanıtı Gönder
        let response_bytes = ok_resp.to_bytes();
        if let Err(e) = self.transport.send(&response_bytes, src_addr).await {
            error!("Failed to send 200 OK: {}", e);
        } else {
            info!("✅ [SIP] 200 OK Sent (RTP Port: {})", rtp_port);
            self.publish_call_started(&call_id, rtp_port, &rtp_target_str, &from, &to).await;
        }

        self.play_welcome_announcement(rtp_port, &call_id, rtp_target_str).await;
    }

    async fn handle_ack(&self, req: SipPacket) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            session.state = CallState::Established;
            info!("✅ [ACK] Call Established: {}", call_id);

            if session.is_bridged {
                // BRIDGING: ACK'ı diğer tarafa ilet
                let target = if session.callee_addr.is_some() { session.callee_addr } else { session.caller_addr };
                if let Some(addr) = target {
                    info!("🔄 [BRIDGING] Relaying ACK to {}", addr);
                    let ack_b = req.clone();
                    // Via ve Route düzenlemeleri gerekebilir ama şimdilik doğrudan gönderiyoruz.
                    let _ = self.transport.send(&ack_b.to_bytes(), addr).await;
                }
            }
        }
    }

    async fn handle_bye(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        info!("🛑 [BYE] Request received for {}", call_id);

        // 200 OK yanıtını gönderen tarafa dön
        let mut ok = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers(&mut ok, &req);
        let _ = self.transport.send(&ok.to_bytes(), src_addr).await;

        if let Some((_, session)) = self.calls.remove(&call_id) {
            if session.is_bridged {
                // BRIDGING: BYE'ı diğer tarafa ilet
                let target = if Some(src_addr) == session.caller_addr { session.callee_addr } else { session.caller_addr };
                if let Some(addr) = target {
                    info!("🔄 [BRIDGING] Relaying BYE to {}", addr);
                    let mut bye_b = req.clone();
                    // Via'yı temizle
                    if !bye_b.headers.is_empty() && bye_b.headers[0].name == HeaderName::Via {
                        bye_b.headers.remove(0);
                    }
                    let _ = self.transport.send(&bye_b.to_bytes(), addr).await;
                }
            }

            if session.rtp_port > 0 {
                self.release_media_port(session.rtp_port).await;
            }
            
            // EVENT PUBLISHING
            self.publish_call_ended(&call_id).await;
        }
    }

    // --- Helpers ---

    async fn publish_call_started(&self, call_id: &str, server_port: u32, caller_rtp: &str, from: &str, to: &str) {
        use sentiric_contracts::sentiric::event::v1::{CallStartedEvent, MediaInfo};
        use prost::Message;

        let event = CallStartedEvent {
            event_type: "call.started".to_string(),
            trace_id: uuid::Uuid::new_v4().to_string(), // Basitlik için
            call_id: call_id.to_string(),
            from_uri: from.to_string(),
            to_uri: to.to_string(),
            timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
            dialplan_resolution: None, // Şimdilik boş
            media_info: Some(MediaInfo {
                caller_rtp_addr: caller_rtp.to_string(),
                server_rtp_port: server_port,
            }),
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
                    if let Ok(parsed_ip) = ip_str.parse::<IpAddr>() {
                        ip = Some(parsed_ip);
                    }
                }
            }
            if line.starts_with("m=audio") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(p) = parts[1].parse::<u16>() {
                        port = Some(p);
                    }
                }
            }
        }
        if let (Some(i), Some(p)) = (ip, port) { Some((i, p)) } else { None }
    }

    async fn allocate_media_port(&self, call_id: &str) -> anyhow::Result<u32> {
        let mut clients = self.clients.lock().await;
        let req = Request::new(AllocatePortRequest {
            call_id: call_id.to_string(),
        });
        let resp = clients.media.allocate_port(req).await?.into_inner();
        Ok(resp.rtp_port)
    }

    async fn release_media_port(&self, port: u32) {
        let mut clients = self.clients.lock().await;
        let req = Request::new(ReleasePortRequest {
            rtp_port: port,
        });
        if let Err(e) = clients.media.release_port(req).await {
            error!("Failed to release port {}: {}", port, e);
        } else {
            info!("♻️ Released RTP Port: {}", port);
        }
    }

    async fn play_welcome_announcement(&self, rtp_port: u32, _call_id: &str, target_addr: String) {
        let mut clients = self.clients.lock().await;
        let audio_path = "audio/tr/system/connecting.wav"; 
        
        let req = Request::new(PlayAudioRequest {
            audio_uri: format!("file://{}", audio_path),
            server_rtp_port: rtp_port,
            rtp_target_addr: target_addr,
        });
        
        match clients.media.play_audio(req).await {
            Ok(_) => info!("🎵 [MEDIA] Announcement started: {}", audio_path),
            Err(e) => error!("❌ [MEDIA] Failed to play announcement: {}", e),
        }
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
    
    pub async fn initiate_call(&self, call_id: String, from: String, to: String) -> anyhow::Result<()> {
        info!("🚀 [OUTBOUND] Call initiation requested. ID: {}, From: {}, To: {}", call_id, from, to);
        // Gelecekte burada outbound call mantığı (Proxy'ye INVITE gönderme) olacak.
        Ok(())
    }
}