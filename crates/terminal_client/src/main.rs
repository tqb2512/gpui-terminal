use std::process::ExitCode;

#[cfg(windows)]
mod workspace;

#[cfg(windows)]
fn main() -> ExitCode {
    use gpui::{
        AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
    };
    use std::{cell::Cell, rc::Rc};
    use workspace::Workspace;

    let exit_code = Rc::new(Cell::new(ExitCode::SUCCESS));
    Application::new().run({
        let exit_code = exit_code.clone();
        move |cx| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(800.0), px(500.0)), cx);
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(320.0), px(200.0))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("GPUI Terminal Client".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Workspace),
            );
            if let Err(error) = window {
                eprintln!("Could not open the terminal window: {error:#}");
                exit_code.set(ExitCode::FAILURE);
                cx.quit();
                return;
            }

            cx.activate(true);
        }
    });
    exit_code.get()
}

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("GPUI Terminal Client currently runs only on Windows.");
    ExitCode::FAILURE
}
