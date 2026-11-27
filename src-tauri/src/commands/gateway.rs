use crate::modules::unified_gateway;

#[derive(serde::Serialize)]
pub struct GatewayStatus {
    pub running: bool,
    pub port: u16,
}

#[tauri::command]
pub async fn gateway_start() -> Result<GatewayStatus, String> {
    let port = unified_gateway::start()?;
    Ok(GatewayStatus {
        running: unified_gateway::is_running(),
        port,
    })
}

#[tauri::command]
pub async fn gateway_status() -> Result<GatewayStatus, String> {
    Ok(GatewayStatus {
        running: unified_gateway::is_running(),
        port: unified_gateway::current_port(),
    })
}

#[tauri::command]
pub async fn gateway_base_url() -> Result<String, String> {
    let port = unified_gateway::current_port();
    Ok(format!("http://127.0.0.1:{port}/v1"))
}
