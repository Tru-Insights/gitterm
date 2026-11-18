// Standalone native registration probe; never link both backends into either app.
// Requires dependency aliases objc2_old = objc2 0.5 and objc2_new = objc2 0.6,
// plus wry 0.48.1 and lb-wry 0.53.3. See ../README.md for resolver constraints.
fn old() {
    let class = <wry::WryWebView as objc2_old::ClassType>::class();
    println!("wry 0.48 registered {}", class.name());
}

fn new() {
    let class = <lb_wry::WryWebView as objc2_new::ClassType>::class();
    println!("lb-wry 0.53 registered {}", class.name().to_string_lossy());
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("old") => old(),
        Some("new") => new(),
        Some("old-first") => {
            old();
            new();
        }
        Some("new-first") => {
            new();
            old();
        }
        _ => panic!("expected old, new, old-first, or new-first"),
    }
}
