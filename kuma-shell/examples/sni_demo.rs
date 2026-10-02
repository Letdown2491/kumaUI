//! A StatusNotifierItem demo client: the tray's `notify-send`.
//!
//! Registers one item (theme icon, tooltip, Activate/SecondaryActivate)
//! with the running watcher so the bar's tray widget can be exercised
//! end-to-end. Run it on the host, click the icon in the bar, watch stdout:
//!
//!   cargo run -p kuma-shell --release --example sni_demo

use zbus::connection;

struct DemoItem {
    activations: u32,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl DemoItem {
    #[zbus(property)]
    fn category(&self) -> &str {
        "SystemServices"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "kuma-sni-demo"
    }

    #[zbus(property)]
    fn title(&self) -> &str {
        "kuma SNI demo"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "bluetooth"
    }

    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }

    #[zbus(property)]
    fn icon_pixmap(&self) -> (i32, i32, Vec<u8>) {
        (0, 0, Vec::new())
    }

    #[zbus(property)]
    fn tool_tip(&self) -> (String, (i32, i32, Vec<u8>), String, String) {
        (
            "bluetooth".to_string(),
            (0, 0, Vec::new()),
            "kuma SNI demo".to_string(),
            "click me: activations print to stdout".to_string(),
        )
    }

    async fn activate(&mut self, x: i32, y: i32) {
        self.activations += 1;
        println!("Activate #{} at ({x}, {y})", self.activations);
    }

    async fn secondary_activate(&mut self, x: i32, y: i32) {
        println!("SecondaryActivate at ({x}, {y})");
    }
}

fn main() {
    let result: Result<(), zbus::Error> = smol::block_on(async {
        let conn = connection::Builder::session()?
            .name("org.kde.StatusNotifierItem-demo-1")?
            .serve_at("/StatusNotifierItem", DemoItem { activations: 0 })?
            .build()
            .await?;

        let watcher = zbus::Proxy::new(
            &conn,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )
        .await?;
        watcher
            .call_method(
                "RegisterStatusNotifierItem",
                &"org.kde.StatusNotifierItem-demo-1",
            )
            .await?;

        println!("SNI demo item registered, click it in the bar; Ctrl+C to exit");
        Ok(smol::future::pending::<()>().await)
    });
    if let Err(err) = result {
        eprintln!("sni_demo: {err:#}");
        std::process::exit(1);
    }
}
