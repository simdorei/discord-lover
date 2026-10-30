use super::{ComponentError, valid_fingerprint};
use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle, Component};

/// Preserve all 25 choices, reserving room for the explicit free-text action.
pub fn async_answer_rows(id: &str, options: &[String]) -> Result<Vec<Component>, ComponentError> {
    let mut rows = async_choice_rows(id, options)?;
    if options.len() == 25 {
        let values = options.iter().enumerate().map(|(index, option)| serde_json::json!({
            "label":format!("{}. {}", index + 1, option).chars().take(100).collect::<String>(),
            "value":index.to_string()
        })).collect::<Vec<_>>();
        rows = vec![
            serde_json::from_value(serde_json::json!({"type":1,"components":[{
                "type":3,"custom_id":format!("codex_async_select:{id}"),"min_values":1,
                "max_values":1,"options":values,"placeholder":"답변 선택"
            }]}))
            .map_err(|_| ComponentError::Invalid)?,
        ];
    }
    let other = Component::Button(Button {
        id: None,
        custom_id: Some(format!("codex_async_other:{id}")),
        disabled: false,
        emoji: None,
        label: Some("다른 답변 입력".into()),
        style: ButtonStyle::Secondary,
        url: None,
        sku_id: None,
    });
    if let Some(Component::ActionRow(row)) = rows.last_mut()
        && row.components.len() < 5
        && row
            .components
            .iter()
            .all(|c| matches!(c, Component::Button(_)))
    {
        row.components.push(other);
    } else {
        rows.push(Component::ActionRow(ActionRow {
            id: None,
            components: vec![other],
        }));
    }
    Ok(rows)
}

pub fn async_choice_rows(id: &str, options: &[String]) -> Result<Vec<Component>, ComponentError> {
    if !valid_fingerprint(id, 64)
        || !(1..=25).contains(&options.len())
        || options.iter().any(|option| option.trim().is_empty())
    {
        return Err(ComponentError::Invalid);
    }
    let buttons = options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            // Full labels stay in the question body. Numbering disambiguates even
            // identical/truncated labels; the ID carries only an immutable index.
            let label = format!("{}. {}", index + 1, option);
            let label = if label.chars().count() > 80 {
                format!("{}…", label.chars().take(79).collect::<String>())
            } else {
                label
            };
            Component::Button(Button {
                id: None,
                custom_id: Some(format!("codex_async:{id}:{index}")),
                disabled: false,
                emoji: None,
                label: Some(label),
                style: ButtonStyle::Primary,
                url: None,
                sku_id: None,
            })
        })
        .collect::<Vec<_>>();
    Ok(buttons
        .chunks(5)
        .map(|buttons| {
            Component::ActionRow(ActionRow {
                id: None,
                components: buttons.to_vec(),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_input_fits_discord_rows_without_losing_any_of_25_choices() {
        let id = "a".repeat(64);
        for count in 1..=25 {
            let options: Vec<_> = (0..count).map(|i| format!("option {i}")).collect();
            let rows = serde_json::to_value(async_answer_rows(&id, &options).unwrap()).unwrap();
            assert!(rows.as_array().unwrap().len() <= 5);
            let mut choices = 0;
            let mut other = 0;
            for row in rows.as_array().unwrap() {
                assert!(row["components"].as_array().unwrap().len() <= 5);
                for component in row["components"].as_array().unwrap() {
                    let custom = component["custom_id"].as_str().unwrap();
                    if custom.starts_with("codex_async_other:") {
                        other += 1;
                    } else if custom.starts_with("codex_async_select:") {
                        choices += component["options"].as_array().unwrap().len();
                    } else if custom.starts_with("codex_async:") {
                        choices += 1;
                    }
                }
            }
            assert_eq!(choices, count);
            assert_eq!(other, 1);
        }
    }
}
