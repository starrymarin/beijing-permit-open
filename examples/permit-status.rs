use beijing_permit_open::{AppSession, PermitClient};
use std::env;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = AppSession::from_token(&env::var("BJ_PERMIT_APP_TOKEN")?)?;
    let mut permit = PermitClient::new(
        session,
        &env::var("BJ_PERMIT_STATE")?,
        &env::var("BJ_PERMIT_SOURCE")?,
    )?;

    let vehicles = permit.vehicles().await?;
    let states = permit.states().await?;
    let rows = states["bzclxx"].as_array().map_or(0, Vec::len);
    // Avoid printing license plates, identity information or upstream response bodies.
    println!(
        "已绑定车辆 {} 辆，办理状态返回 {rows} 个车辆条目。",
        vehicles.len()
    );
    Ok(())
}
