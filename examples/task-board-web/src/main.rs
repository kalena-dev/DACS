#[cfg(any(target_arch = "wasm32", test))]
mod api;
#[cfg(target_arch = "wasm32")]
mod app;

#[cfg(target_arch = "wasm32")]
fn main() {
    leptos::mount::mount_to_body(app::App);
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!(
        "This is a browser-only application. Run `trunk build --release` in examples/task-board-web and serve dist with `dacs task-board --assets <dist>`."
    );
    std::process::exit(1);
}
