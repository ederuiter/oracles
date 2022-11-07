pub mod hybrid;
pub mod iot;
pub mod mobile;

pub use hybrid::hybrid;
pub type GrpcResult<T> = std::result::Result<tonic::Response<T>, tonic::Status>;
pub type VerifyResult<T> = std::result::Result<T, tonic::Status>;
