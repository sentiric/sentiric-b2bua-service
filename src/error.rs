// sentiric-b2bua-service/src/error.rs
use thiserror::Error;
use tonic::Status;

#[derive(Error, Debug)]
pub enum ServiceError {
    #[error("Yapılandırma/Ortam hatası: {0}")]
    ConfigError(#[from] anyhow::Error),
    #[error("gRPC iletişim hatası: {0}")]
    GrpcTransportError(#[from] tonic::transport::Error),
    #[error("gRPC servis hatası: {0}")]
    GrpcStatus(#[from] tonic::Status),
    #[error("SIP sinyalleşme hatası: {0}")]
    SipSignalingError(String),
    #[error("I/O hatası: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ServiceError> for Status {
    fn from(err: ServiceError) -> Self {
        match err {
            ServiceError::GrpcStatus(s) => s,
            ServiceError::SipSignalingError(msg) => Status::unavailable(format!("SIP Sinyalleşme Başarısız: {}", msg)),
            ServiceError::GrpcTransportError(e) => Status::unavailable(format!("gRPC bağlantı hatası: {}", e)),
            _ => Status::internal(format!("{:#?}", err)), 
        }
    }
}