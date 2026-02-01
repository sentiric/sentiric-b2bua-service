// sentiric-b2bua-service/src/grpc/client.rs

use crate::config::AppConfig;
use anyhow::Result;
use sentiric_contracts::sentiric::media::v1::media_service_client::MediaServiceClient;
use sentiric_contracts::sentiric::sip::v1::registrar_service_client::RegistrarServiceClient;
use sentiric_contracts::sentiric::dialplan::v1::dialplan_service_client::DialplanServiceClient;
use tonic::transport::{Channel, ClientTlsConfig, Certificate, Identity};
use std::time::Duration;
use tracing::info;

pub struct InternalClients {
    pub media: MediaServiceClient<Channel>,
    pub registrar: RegistrarServiceClient<Channel>,
    pub user: sentiric_contracts::sentiric::user::v1::user_service_client::UserServiceClient<Channel>,
    pub dialplan: DialplanServiceClient<Channel>, // YENİ
}

impl InternalClients {
    pub async fn connect(config: &AppConfig) -> Result<Self> {
        info!("İç servislere bağlanılıyor...");

        let media_channel = create_secure_channel(&config.media_service_url, "media-service", config).await?;
        let registrar_channel = create_secure_channel(&config.registrar_service_url, "registrar-service", config).await?;
        let user_channel = create_secure_channel(&config.user_service_url, "user-service", config).await?;
        let dialplan_channel = create_secure_channel(&config.dialplan_service_url, "dialplan-service", config).await?; // YENİ

        Ok(Self {
            media: MediaServiceClient::new(media_channel),
            registrar: RegistrarServiceClient::new(registrar_channel),
            user: sentiric_contracts::sentiric::user::v1::user_service_client::UserServiceClient::new(user_channel),
            dialplan: DialplanServiceClient::new(dialplan_channel), // YENİ
        })
    }
}

async fn create_secure_channel(url: &str, server_name: &str, config: &AppConfig) -> Result<Channel> {
    let target_url = if url.starts_with("http") {
        url.to_string()
    } else {
        format!("https://{}", url)
    };

    let cert = tokio::fs::read(&config.cert_path).await?;
    let key = tokio::fs::read(&config.key_path).await?;
    let identity = Identity::from_pem(cert, key);
    let ca_cert = tokio::fs::read(&config.ca_path).await?;
    let ca_certificate = Certificate::from_pem(ca_cert);

    let tls_config = ClientTlsConfig::new()
        .domain_name(server_name)
        .ca_certificate(ca_certificate)
        .identity(identity);

    let channel = Channel::from_shared(target_url)?
        .connect_timeout(Duration::from_secs(5))
        .tls_config(tls_config)?
        .connect()
        .await?;

    Ok(channel)
}