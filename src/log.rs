use napi_derive::napi;

#[napi]
pub mod log {
    #[napi]
    pub fn init_logger(app_data: String) -> String {
        logrs::init_logger(app_data,"sanwatcher.log".to_string())
    }

    #[napi]
    pub fn test_panic() {
        panic!("This is a test panic");
    }
}