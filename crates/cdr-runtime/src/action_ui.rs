use cdr_discord::components::{ComponentError, busy_button_row};
use twilight_model::channel::message::Component;

use crate::action_executor::ActionUi;

pub fn render_action_ui(ui: Option<&ActionUi>) -> Result<Vec<Component>, ComponentError> {
    match ui {
        Some(ActionUi::PendingBusy { choice_id }) => {
            let Component::ActionRow(mut row) = busy_button_row(choice_id, false)? else {
                return Err(ComponentError::Invalid);
            };
            row.components.retain(|component| {
                matches!(component, Component::Button(button) if button.custom_id.as_deref()
                    .is_some_and(|id| id.ends_with(":queue") || id.ends_with(":ignore")))
            });
            Ok(vec![Component::ActionRow(row)])
        }
        Some(ActionUi::ProBusy { choice_id }) => {
            let Component::ActionRow(mut row) = busy_button_row(choice_id, false)? else {
                return Err(ComponentError::Invalid);
            };
            row.components.retain(|component| {
                !matches!(component,
                Component::Button(button) if button.custom_id.as_deref()
                    .is_some_and(|id| id.ends_with(":steer")))
            });
            Ok(vec![Component::ActionRow(row)])
        }
        Some(ActionUi::ServerPrompts { prompts }) => {
            let rows = prompts
                .iter()
                .flat_map(|prompt| prompt.prompt.components.iter().cloned())
                .collect::<Vec<_>>();
            if rows.len() > 5 {
                return Err(ComponentError::Invalid);
            }
            Ok(rows)
        }
        Some(ActionUi::Busy {
            choice_id,
            allow_steer,
        }) => Ok(vec![busy_button_row(choice_id, *allow_steer)?]),
        None => Ok(Vec::new()),
    }
}
