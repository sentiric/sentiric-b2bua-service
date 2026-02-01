// sentiric-b2bua-service/src/config.rs
use anyhow::{Context, Result};
use std::env;
use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub grpc_listen_addr: SocketAddr,
    pub http_listen_addr: SocketAddr,
    
    // SIP Network
    pub sip_bind_ip: String,
    pub sip_port: u16,
    
    // Dependencies
    pub media_service_url: String,
    pub proxy_service_url: String, 
    pub registrar_service_url: String,
    pub user_service_url: String,
    pub rabbitmq_url: String,
    pub dialplan_service_url: String, // YENİ
    
    pub proxy_sip_addr: String,
    
    pub public_ip: String, 
    
    pub vendor_profile: String,
    pub env: String,
    pub rust_log: String,
    pub service_version: String,
    
    // SIP Settings
    pub sip_realm: String,
    pub welcome_audio_path: String,
    
    pub cert_path: String,
    pub key_path: String,
    pub ca_path: String,
}

impl AppConfig {
    pub fn load_from_env() -> Result<Self> {
        let grpc_port = env::var("B2BUA_SERVICE_GRPC_PORT").unwrap_or_else(|_| "13081".to_string());
        let http_port = env::var("B2BUA_SERVICE_HTTP_PORT").unwrap_or_else(|_| "13080".to_string());
        
        let sip_port_str = env::var("B2BUA_SERVICE_SIP_PORT").unwrap_or_else(|_| "13084".to_string());
        let sip_port = sip_port_str.parse::<u16>().context("Geçersiz SIP portu")?;

        let grpc_addr: SocketAddr = format!("[::]:{}", grpc_port).parse()?;
        let http_addr: SocketAddr = format!("[::]:{}", http_port).parse()?;
        
        let proxy_target = env::var("PROXY_SERVICE_SIP_TARGET")
            .unwrap_or_else(|_| "proxy-service:13074".to_string());

        let public_ip = env::var("B2BUA_SERVICE_PUBLIC_IP")
            .or_else(|_| env::var("NODE_IP")) 
            .context("❌ FATAL: B2BUA_SERVICE_PUBLIC_IP veya NODE_IP tanımlı değil!")?;
       
        if public_ip.is_empty() || public_ip == "127.0.0.1" {
            anyhow::bail!("❌ FATAL: PUBLIC_IP geçersiz (boş veya localhost): {}", public_ip);
        }
        
        public_ip.parse::<std::net::IpAddr>()
            .context(format!("❌ PUBLIC_IP geçersiz format: {}", public_ip))?;
        
        tracing::info!("✅ B2BUA Public IP validated: {}", public_ip);

        Ok(AppConfig {
            grpc_listen_addr: grpc_addr,
            http_listen_addr: http_addr, 

            sip_bind_ip: "0.0.0.0".to_string(),
            sip_port,
            proxy_sip_addr: proxy_target,

            media_service_url: env::var("MEDIA_SERVICE_TARGET_GRPC_URL").context("ZORUNLU: MEDIA_SERVICE_TARGET_GRPC_URL")?,
            proxy_service_url: env::var("PROXY_SERVICE_TARGET_GRPC_URL").unwrap_or_default(),
            registrar_service_url: env::var("REGISTRAR_SERVICE_TARGET_GRPC_URL").context("ZORUNLU: REGISTRAR_SERVICE_TARGET_GRPC_URL")?,
            user_service_url: env::var("USER_SERVICE_TARGET_GRPC_URL").unwrap_or_else(|_| "https://user-service:12011".to_string()),
            rabbitmq_url: env::var("RABBITMQ_URL").context("ZORUNLU: RABBITMQ_URL")?,
            dialplan_service_url: env::var("DIALPLAN_SERVICE_TARGET_GRPC_URL").context("ZORUNLU: DIALPLAN_SERVICE_TARGET_GRPC_URL")?, // YENİ
            
            public_ip,
            
            vendor_profile: env::var("VENDOR_PROFILE").unwrap_or_else(|_| "legacy".to_string()),
            
            sip_realm: env::var("SIP_REALM").unwrap_or_else(|_| "sip.azmisahin.com".to_string()),
            welcome_audio_path: env::var("WELCOME_AUDIO_PATH").unwrap_or_else(|_| "audio/tr/system/connecting.wav".to_string()),

            env: env::var("ENV").unwrap_or_else(|_| "production".to_string()),
            rust_log: env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
            service_version: env::var("SERVICE_VERSION").unwrap_or_else(|_| "1.0.0".to_string()),
            
            cert_path: env::var("B2BUA_SERVICE_CERT_PATH").context("ZORUNLU: B2BUA_SERVICE_CERT_PATH")?,
            key_path: env::var("B2BUA_SERVICE_KEY_PATH").context("ZORUNLU: B2BUA_SERVICE_KEY_PATH")?,
            ca_path: env::var("GRPC_TLS_CA_PATH").context("ZORUNLU: GRPC_TLS_CA_PATH")?,
        })
    }
}