use super::*;

impl App {
    pub(super) fn start_automode(
        &mut self,
        tui: &mut tui::Tui,
        request: crate::automode::AutomodeStartRequest,
    ) {
        if self.automode.session.is_some() {
            self.chat_widget.add_error_message(
                "Automode is already running. Use /automode stop first.".to_string(),
            );
            tui.frame_requester().schedule_frame();
            return;
        }
        if self.chat_widget.is_user_turn_pending_or_running() {
            self.chat_widget.add_error_message(
                "Automode cannot start while a turn is already running.".to_string(),
            );
            tui.frame_requester().schedule_frame();
            return;
        }

        let run_id = self.automode.allocate_run_id();
        let mut session = match crate::automode::AutomodeRunState::start(request, run_id) {
            Ok(session) => session,
            Err(message) => {
                self.chat_widget.add_error_message(message);
                tui.frame_requester().schedule_frame();
                return;
            }
        };

        let deadline = session.deadline();
        let tx = self.app_event_tx.clone();
        self.automode.clear_deadline_task();
        self.automode.deadline_task = Some(tokio::spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            tx.send(AppEvent::AutomodeDeadlineReached { run_id });
        }));

        self.chat_widget
            .add_plain_history_lines(crate::automode::automode_started_lines(&session));
        if self.submit_next_automode_turn(&mut session) {
            self.automode.session = Some(session);
        } else {
            self.automode.clear_deadline_task();
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn stop_automode(&mut self, tui: &mut tui::Tui) {
        let Some(session) = self.automode.session.take() else {
            self.chat_widget
                .add_info_message("Automode is not running.".to_string(), /*hint*/ None);
            tui.frame_requester().schedule_frame();
            return;
        };

        self.automode.clear_deadline_task();
        self.chat_widget
            .add_plain_history_lines(crate::automode::automode_stopped_lines(Some(
                session.progress_path(),
            )));
        if self.chat_widget.is_user_turn_pending_or_running() {
            self.app_event_tx.interrupt();
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn handle_automode_deadline_reached(&mut self, tui: &mut tui::Tui, run_id: u64) {
        let Some(session) = self.automode.session.take() else {
            return;
        };
        if session.run_id() != run_id {
            self.automode.session = Some(session);
            return;
        }

        self.automode.clear_deadline_task();
        self.chat_widget
            .add_plain_history_lines(crate::automode::automode_finished_lines(
                session.progress_path(),
            ));
        if self.chat_widget.is_user_turn_pending_or_running() {
            self.app_event_tx.interrupt();
        }
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn handle_automode_turn_completed(
        &mut self,
        tui: &mut tui::Tui,
        status: TurnStatus,
    ) {
        let Some(mut session) = self.automode.session.take() else {
            return;
        };

        match status {
            TurnStatus::Interrupted => {
                self.automode.clear_deadline_task();
                let lines = if session.deadline_reached() {
                    crate::automode::automode_finished_lines(session.progress_path())
                } else {
                    crate::automode::automode_stopped_lines(Some(session.progress_path()))
                };
                self.chat_widget.add_plain_history_lines(lines);
            }
            TurnStatus::Completed | TurnStatus::Failed => {
                if session.deadline_reached() {
                    self.automode.clear_deadline_task();
                    self.chat_widget.add_plain_history_lines(
                        crate::automode::automode_finished_lines(session.progress_path()),
                    );
                } else {
                    let keep_running = self.chat_widget.is_user_turn_pending_or_running()
                        || self.submit_next_automode_turn(&mut session);
                    if keep_running {
                        self.automode.session = Some(session);
                    } else {
                        self.automode.clear_deadline_task();
                    }
                }
            }
            TurnStatus::InProgress => {
                self.automode.session = Some(session);
            }
        }

        tui.frame_requester().schedule_frame();
    }

    fn submit_next_automode_turn(
        &mut self,
        session: &mut crate::automode::AutomodeRunState,
    ) -> bool {
        let turn_prompt = session.next_turn_prompt();
        if !self.chat_widget.submit_automode_turn_prompt(turn_prompt) {
            self.chat_widget
                .add_error_message("Automode failed to submit the next turn.".to_string());
            return false;
        }
        true
    }
}
