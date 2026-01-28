// sentiric-b2bua-service/src/sip/server.rs

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn, debug};
use sentiric_sip_core::{SipTransport, parser};
use crate::sip::engine::B2BuaEngine;

pub struct SipServer {
    engine: Arc<B2BuaEngine>,
    transport: Arc<SipTransport>,
}

impl SipServer {
    pub fn new(engine: Arc<B2BuaEngine>, transport: Arc<SipTransport>) -> Self {
        Self { engine, transport }
    }

    pub async fn run(self, mut shutdown_rx: mpsc::Receiver<()>) {
        info!("📡 B2BUA SIP Listener aktif.");

        let mut buf = vec![0u8; 65535];
        let socket = self.transport.get_socket();

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("🛑 SIP Server kapatılıyor...");
                    break;
                }
                
                res = socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, src_addr)) => {
                            // [FIX] Keep-Alive Filtresi
                            if len < 4 || buf[..len].iter().all(|&b| b == b'\r' || b == b'\n' || b == 0) {
                                debug!("💤 Keep-Alive ignored from {}", src_addr);
                                continue;
                            }

                            let data = &buf[..len];
                            match parser::parse(data) {
                                Ok(packet) => {
                                    let engine = self.engine.clone();
                                    tokio::spawn(async move {
                                        engine.handle_packet(packet, src_addr).await;
                                    });
                                },
                                Err(e) => {
                                    warn!("SIP parse hatası: {} (Len: {})", e, len);
                                }
                            }
                        },
                        Err(e) => error!("UDP Error: {}", e),
                    }
                }
            }
        }
    }
}