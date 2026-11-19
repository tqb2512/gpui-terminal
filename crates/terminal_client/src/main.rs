use std::process::ExitCode;

#[cfg(windows)]
mod files_view;
#[cfg(windows)]
mod profile_form;
#[cfg(windows)]
mod windows_open_with;
#[cfg(windows)]
mod workspace;

#[cfg(all(windows, test))]
mod files_view_test {
    use std::{cell::Cell, fs};

    use crate::files_view::{Request, accept_opened, write_warning};
    use crate::windows_open_with::OpenWithOutcome;
    use terminal_client::FileError;

    #[test]
    fn files_discards_stale_results() {
        let first = Request::default();
        let prepared = first.clone();
        let replacement = Request::default();
        first.invalidate();
        assert!(!prepared.is_current());
        assert!(replacement.is_current());
    }

    #[test]
    fn files_disconnected_write_still_has_a_warning() {
        let request = Request::default();
        request.invalidate();
        assert!(!request.is_current());
        let warning = write_warning(&FileError::Remote(ssh::SftpError::OutcomeUnknown));
        assert!(warning.is_some_and(|message| message.contains("Check the remote file")));
        assert!(write_warning(&FileError::AttemptEnded).is_none());
    }

    #[test]
    fn open_with_cancel_does_not_watch_or_keep_unlaunched_copy()
    -> Result<(), Box<dyn std::error::Error>> {
        struct LocalCopy(tempfile::TempDir);
        let copy = LocalCopy(tempfile::tempdir()?);
        let path = copy.0.path().join("edit.txt");
        fs::write(&path, b"example")?;
        let called = Cell::new(false);
        assert!(
            accept_opened(OpenWithOutcome::Cancelled, copy, |_| {
                called.set(true);
            })
            .is_none()
        );
        assert!(!called.get());
        assert!(!path.exists());
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> ExitCode {
    use gpui::{
        AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
    };
    use std::{cell::Cell, rc::Rc};
    use workspace::Workspace;

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Could not start the SSH runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let handle = runtime.handle().clone();
    let exit_code = Rc::new(Cell::new(ExitCode::SUCCESS));
    Application::new().run({
        let exit_code = exit_code.clone();
        move |cx| {
            terminal_view::bind_keys(cx);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = cx.primary_display().map(|display| {
                let display_bounds = display.bounds();
                let [width, height] = initial_window_size([
                    display_bounds.size.width.into(),
                    display_bounds.size.height.into(),
                ]);
                WindowBounds::Windowed(Bounds::centered_at(
                    display_bounds.center(),
                    size(px(width), px(height)),
                ))
            });
            let window = cx.open_window(
                WindowOptions {
                    window_bounds: bounds,
                    window_min_size: Some(size(px(320.0), px(200.0))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("GPUI Terminal Client".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Workspace::new(handle, window, cx)),
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

#[cfg(any(windows, test))]
fn initial_window_size([width, height]: [f32; 2]) -> [f32; 2] {
    // GPUI reports full monitor bounds, so leave room for the frame and taskbar.
    [800.0_f32.min(width * 0.8), 500.0_f32.min(height * 0.8)]
}

#[cfg(test)]
mod tests {
    use super::initial_window_size;

    #[test]
    fn leaves_room_around_the_window_on_a_small_high_dpi_display() {
        assert_eq!(initial_window_size([640.0, 360.0]), [512.0, 288.0]);
    }

    #[test]
    fn keeps_the_preferred_size_on_a_large_display() {
        assert_eq!(initial_window_size([1920.0, 1080.0]), [800.0, 500.0]);
    }
}
