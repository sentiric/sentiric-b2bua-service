// sentiric-b2bua-service/src/sip/engine.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error, instrument};
use sentiric_sip_core::{SipPacket, Method, Header, HeaderName, SipTransport};
use crate::config::AppConfig;
use crate::grpc::client::InternalClients;
use crate::sip::state::{CallStore, CallSession, CallState};
use sentiric_contracts::sentiric::media::v1::AllocatePortRequest;
use tonic::Request;
use tokio::net::lookup_host; // DÜZELTME: Eklendi

pub struct B2BuaEngine {
    config: Arc<AppConfig>,
    clients: Arc<Mutex<InternalClients>>,
    calls: CallStore,
    transport: Arc<SipTransport>,
}

impl B2BuaEngine {
    pub fn new(
        config: Arc<AppConfig>, 
        clients: Arc<Mutex<InternalClients>>, 
        calls: CallStore,
        transport: Arc<SipTransport>
    ) -> Self {
        Self { config, clients, calls, transport }
    }

    /// Yeni bir dış çağrı başlatır (Outbound).
    #[instrument(skip(self))]
    pub async fn initiate_call(&self, call_id: String, from: String, to: String) -> anyhow::Result<()> {
        info!("B2BUA: Çağrı başlatılıyor: {} -> {} (ID: {})", from, to, call_id);

        // 1. RTP Portu Al
        let mut clients = self.clients.lock().await;
        let port_res = clients.media.allocate_port(Request::new(AllocatePortRequest {
            call_id: call_id.clone(),
        })).await?;
        let rtp_port = port_res.into_inner().rtp_port;
        drop(clients); // Mutex'i bırak

        // 2. SDP Oluştur
        let sdp = format!(
            "v=0\r\n\
            o=- {0} {0} IN IP4 {1}\r\n\
            s=Sentiric_B2BUA\r\n\
            c=IN IP4 {1}\r\n\
            t=0 0\r\n\
            m=audio {2} RTP/AVP 18 8 0 101\r\n\
            a=rtpmap:18 G729/8000\r\n\
            a=rtpmap:8 PCMA/8000\r\n\
            a=rtpmap:0 PCMU/8000\r\n\
            a=rtpmap:101 telephone-event/8000\r\n\
            a=fmtp:101 0-16\r\n\
            a=sendrecv\r\n",
            123456, // Session ID
            self.config.public_ip,
            rtp_port
        );

        // 3. INVITE Paketi Oluştur
        let mut invite = SipPacket::new_request(Method::Invite, to.clone());
        
        // Headerlar
        invite.headers.push(Header::new(HeaderName::Via, format!("SIP/2.0/UDP {}:{};branch=z9hG4bK-b2bua-{}", self.config.public_ip, self.config.sip_port, call_id)));
        invite.headers.push(Header::new(HeaderName::From, format!("<{}>;tag=b2bua-{}", from, call_id)));
        invite.headers.push(Header::new(HeaderName::To, format!("<{}>", to)));
        invite.headers.push(Header::new(HeaderName::CallId, call_id.clone()));
        invite.headers.push(Header::new(HeaderName::CSeq, "1 INVITE".to_string()));
        invite.headers.push(Header::new(HeaderName::Contact, format!("<sip:b2bua@{}:{}>", self.config.public_ip, self.config.sip_port)));
        invite.headers.push(Header::new(HeaderName::UserAgent, "Sentiric B2BUA".to_string()));
        invite.headers.push(Header::new(HeaderName::ContentType, "application/sdp".to_string()));
        
        invite.body = sdp.as_bytes().to_vec();

        // 4. State Kaydet
        self.calls.insert(call_id.clone(), CallSession {
            call_id: call_id.clone(),
            state: CallState::Trying,
            from_uri: from,
            to_uri: to,
            rtp_port,
            remote_tag: None,
        });

        // 5. Proxy'ye Gönder
        let bytes = invite.to_bytes();
        
        // DÜZELTME: DNS Çözümlemesi
        match lookup_host(&self.config.proxy_sip_addr).await {
            Ok(mut addrs) => {
                if let Some(target_addr) = addrs.next() {
                    self.transport.send(&bytes, target_addr).await?;
                    info!("INVITE gönderildi (Proxy IP: {})", target_addr);
                } else {
                    error!("DNS Çözümlemesi başarısız (boş sonuç): {}", self.config.proxy_sip_addr);
                    return Err(anyhow::anyhow!("DNS Resolution Empty"));
                }
            },
            Err(e) => {
                error!("DNS Çözümleme hatası ({}): {}", self.config.proxy_sip_addr, e);
                return Err(e.into());
            }
        }

        Ok(())
    }

    /// Gelen SIP paketlerini işler (Proxy'den dönen yanıtlar).
    pub async fn handle_packet(&self, packet: SipPacket) {
        if let Some(call_id) = packet.get_header_value(HeaderName::CallId) {
            // Eğer bu Call-ID'ye sahip bir oturumumuz varsa işle
            if let Some(mut session) = self.calls.get_mut(call_id) {
                if !packet.is_request {
                    // YANIT (Response)
                    match packet.status_code {
                        180 | 183 => {
                            info!("Çağrı Çalıyor: {}", call_id);
                            session.state = CallState::Ringing;
                        },
                        200 => {
                            info!("Çağrı Cevaplandı: {}", call_id);
                            session.state = CallState::Established;
                            
                            // To-Tag'i sakla (ACK ve BYE için lazım)
                            if let Some(to) = packet.get_header_value(HeaderName::To) {
                                if let Some(tag_idx) = to.find("tag=") {
                                    session.remote_tag = Some(to[tag_idx+4..].to_string());
                                }
                            }

                            // ACK Gönder
                            self.send_ack(&session, &packet).await;
                        },
                        400..=699 => {
                            warn!("Çağrı Başarısız: {} (Code: {})", call_id, packet.status_code);
                            session.state = CallState::Terminated;
                            // TODO: Kaynakları temizle
                        },
                        _ => {}
                    }
                }
            }
        }
    }

    async fn send_ack(&self, session: &CallSession, _resp: &SipPacket) {
        // let mut ack = SipPacket::new_request(Method::Ack, session.to_uri.clone());
        // Temel headerları kopyala/oluştur...
        // Basitlik için şimdilik sadece logluyoruz. Gerçek implementasyonda RFC uyumlu ACK oluşturulmalı.
        info!("ACK gönderiliyor (TODO Implementation)...");
    }
}