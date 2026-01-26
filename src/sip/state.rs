// sentiric-b2bua-service/src/sip/state.rs

use std::sync::Arc;
use dashmap::DashMap;
// use std::net::SocketAddr; // KALDIRILDI: Kullanılmıyordu

#[derive(Debug, Clone)]
pub struct CallSession {
    pub call_id: String,
    pub state: CallState,
    pub from_uri: String,
    pub to_uri: String,
    
    // Medya Bilgileri (Media Service'ten gelen)
    pub rtp_port: u32,
    
    // SIP Transaction State (Retransmission için kritik)
    pub local_tag: String,
    pub last_response_packet: Option<Vec<u8>>, // Üretilen son 200 OK paketi
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallState {
    Invited,
    Established,
    Terminated,
}

pub type CallStore = Arc<DashMap<String, CallSession>>;

pub fn new_store() -> CallStore {
    Arc::new(DashMap::new())
}