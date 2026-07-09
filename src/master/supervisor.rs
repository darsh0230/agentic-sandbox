#[derive(Debug, Clone, Copy)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}
