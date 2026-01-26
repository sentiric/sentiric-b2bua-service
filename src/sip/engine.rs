// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, warn, debug};
use sentiric_sip_core::{SipPacket, Method, HeaderName, Header, utils as sip_utils};
use sentiric_contracts::sentiric::media::v1::{AllocatePortRequest, PlayAudioRequest, ReleasePortRequest};
use tonic::Request;
use crate::grpc::client::InternalClients;
use crate::config::AppConfig;
use crate::sip::state::{CallStore, CallSession, CallState};
use crate::rabbitmq::RabbitMqClient;
use std::net::SocketAddr;
use uuid::Uuid;

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
                _ => {
                    debug!("Method not handled: {:?}", packet.method);
                }
            }
        } else {
            // Response handling (İleride Outbound aramalar için burası kullanılacak)
            debug!("Response ignored in this phase.");
        }
    }

    /// INVITE İşleme (Çağrı Karşılama - IVR/Echo)
    async fn handle_invite(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        let from = req.get_header_value(HeaderName::From).cloned().unwrap_or_default();
        let to = req.get_header_value(HeaderName::To).cloned().unwrap_or_default();

        // 1. Retransmission Kontrolü (GOLDEN RULE)
        // Eğer bu çağrı için zaten bir session varsa ve cevap üretildiyse, aynısını gönder.
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            if let Some(last_resp) = &session.last_response_packet {
                info!("🔄 [SIP] Retransmission detected for {}, resending 200 OK.", call_id);
                let _ = self.transport.send(last_resp, src_addr).await;
                return;
            }
        }

        info!("📞 [INVITE] New Call: {} -> {}", from, to);

        // 2. 100 Trying Gönder
        let mut trying = SipPacket::new_response(100, "Trying".to_string());
        self.copy_headers(&mut trying, &req);
        let _ = self.transport.send(&trying.to_bytes(), src_addr).await;

        // 3. Media Service'ten Port İste
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

        // 4. Session Oluştur
        let local_tag = sip_utils::generate_tag("b2bua");
        let session = CallSession {
            call_id: call_id.clone(),
            state: CallState::Invited,
            from_uri: from.clone(),
            to_uri: to.clone(),
            rtp_port,
            local_tag: local_tag.clone(),
            last_response_packet: None,
        };
        self.calls.insert(call_id.clone(), session);

        // 5. 200 OK (SDP) Oluştur
        // Not: Burada 'public_ip' config'den geliyor.
        let sdp = format!(
            "v=0\r\n\
            o=- 123456 123456 IN IP4 {}\r\n\
            s=SentiricB2BUA\r\n\
            c=IN IP4 {}\r\n\
            t=0 0\r\n\
            m=audio {} RTP/AVP 0 8 101\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-16\r\n\
            a=sendrecv\r\n",
            self.config.public_ip, 
            self.config.public_ip,
            rtp_port
        );

        let mut ok_resp = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers_with_tag(&mut ok_resp, &req, &local_tag);
        
        // Contact Header (Public IP ile)
        let contact = format!("<sip:b2bua@{}:{}>", self.config.public_ip, self.config.sip_port);
        ok_resp.headers.push(Header::new(HeaderName::Contact, contact));
        ok_resp.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        ok_resp.body = sdp.as_bytes().to_vec();

        let response_bytes = ok_resp.to_bytes();

        // 6. Response'u Cache'le ve Gönder
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            session.last_response_packet = Some(response_bytes.clone());
        }
        
        if let Err(e) = self.transport.send(&response_bytes, src_addr).await {
            error!("Failed to send 200 OK: {}", e);
        } else {
            info!("✅ [SIP] 200 OK Sent (RTP Port: {})", rtp_port);
        }

        // 7. IVR / Anons Başlat (Auto-Play Welcome)
        self.play_welcome_announcement(rtp_port, &call_id).await;
    }

    async fn handle_ack(&self, req: SipPacket) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        if let Some(mut session) = self.calls.get_mut(&call_id) {
            session.state = CallState::Established;
            info!("✅ [ACK] Call Established: {}", call_id);
        }
    }

    async fn handle_bye(&self, req: SipPacket, src_addr: SocketAddr) {
        let call_id = req.get_header_value(HeaderName::CallId).cloned().unwrap_or_default();
        info!("🛑 [BYE] Request received for {}", call_id);

        let mut ok = SipPacket::new_response(200, "OK".to_string());
        self.copy_headers(&mut ok, &req);
        let _ = self.transport.send(&ok.to_bytes(), src_addr).await;

        if let Some((_, session)) = self.calls.remove(&call_id) {
            // Portu serbest bırak
            self.release_media_port(session.rtp_port).await;
        }
    }

    // --- Helpers ---

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

    async fn play_welcome_announcement(&self, rtp_port: u32, _call_id: &str) {
        let mut clients = self.clients.lock().await;
        
        // DÜZELTME: "welcome.wav" yerine, sistemde kesin olarak bulunan
        // "audio/tr/system/connecting.wav" dosyasını kullanıyoruz.
        // Bu dosya altyapı kurulumunda oluşturulan SQL ile tanımlıdır.
        let audio_path = "audio/tr/system/connecting.wav"; 
        
        let req = Request::new(PlayAudioRequest {
            audio_uri: format!("file://{}", audio_path),
            server_rtp_port: rtp_port,
            rtp_target_addr: "0.0.0.0:0".to_string(), // Media Service Latching yapacağı için dummy IP
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
                    // To header'ına tag ekle
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
    
    // GRPC Service üzerinden çağrılan metotlar
    pub async fn initiate_call(&self, _call_id: String, _from: String, _to: String) -> anyhow::Result<()> {
        Ok(())
    }
}