use super::*;

impl App {
    pub(super) fn start_automode(
        &mut self,
        tui: &mut tui::Tui,
        request: crate::automode::AutomodeStartRequest,
    ) {
        if self
            .automode
            .task
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
        {
            self.chat_widget.add_error_message(
                "Automode is already running. Use /automode stop first.".to_string(),
            );
            tui.frame_requester().schedule_frame();
            return;
        }
        self.automode.task = None;

        let command_echo = crate::automode::format_automode_command(&request);
        let mut lines = vec![command_echo.magenta().into()];
        lines.extend(crate::automode::automode_full_access_warning_lines());
        self.chat_widget.add_plain_history_lines(lines);

        let args = request.into_exec_args();
        let arg0_paths = self.automode.arg0_paths.clone();
        let tx = self.app_event_tx.clone();
        let event_tx = tx.clone();
        let sink = codex_exec::AutomodeEventSink::new(move |event| {
            event_tx.send(AppEvent::AutomodeEvent(
                crate::automode::AutomodeUiEvent::Runtime(event),
            ));
        });

        self.automode.task = Some(tokio::spawn(async move {
            if let Err(err) =
                codex_exec::run_automode_with_events(args, arg0_paths, Some(sink)).await
            {
                tx.send(AppEvent::AutomodeEvent(
                    crate::automode::AutomodeUiEvent::Failed {
                        message: err.to_string(),
                    },
                ));
            }
        }));
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn stop_automode(&mut self, tui: &mut tui::Tui) {
        match self.automode.task.take() {
            Some(handle) if !handle.is_finished() => {
                handle.abort();
                self.handle_automode_event(crate::automode::AutomodeUiEvent::Stopped);
            }
            _ => {
                self.chat_widget
                    .add_info_message("Automode is not running.".to_string(), /*hint*/ None);
            }
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn handle_automode_event(&mut self, event: crate::automode::AutomodeUiEvent) {
        let terminal_event = matches!(
            &event,
            crate::automode::AutomodeUiEvent::Runtime(codex_exec::AutomodeEvent::Finished { .. })
                | crate::automode::AutomodeUiEvent::Failed { .. }
                | crate::automode::AutomodeUiEvent::Stopped
        );
        self.chat_widget
            .add_plain_history_lines(crate::automode::render_automode_event(&event));
        if terminal_event {
            self.automode.task = None;
        }
    }
}
