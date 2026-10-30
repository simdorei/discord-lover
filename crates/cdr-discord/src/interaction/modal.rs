use super::{InteractionRoute, InteractionRouteError, RoutedWork};
use crate::components::{ComponentId, deferred_update, parse_component_id};
use twilight_model::application::interaction::modal::{
    ModalInteractionComponent, ModalInteractionData,
};
use twilight_model::http::interaction::InteractionResponse;

pub(super) fn open(question_id: &str) -> Result<InteractionResponse, InteractionRouteError> {
    serde_json::from_value(serde_json::json!({"type":9,"data":{
        "custom_id":format!("codex_async_reply:{question_id}"),"title":"다른 답변 입력",
        "components":[{"type":18,"label":"이 질문에 대한 답변","component":{
            "type":4,"custom_id":"answer","style":2,"min_length":1,"max_length":4000,
            "required":true
        }}]
    }}))
    .map_err(|_| InteractionRouteError::UnknownComponent)
}

pub fn route_modal(data: &ModalInteractionData) -> Result<InteractionRoute, InteractionRouteError> {
    let id = data
        .custom_id
        .strip_prefix("codex_async_reply:")
        .ok_or(InteractionRouteError::UnknownComponent)?;
    let Some(ComponentId::AsyncTextInput { question_id }) =
        parse_component_id(&format!("codex_async_other:{id}"))
    else {
        return Err(InteractionRouteError::UnknownComponent);
    };
    let [layout] = data.components.as_slice() else {
        return Err(InteractionRouteError::UnknownComponent);
    };
    let input = match layout {
        ModalInteractionComponent::Label(label) => label.component.as_ref(),
        ModalInteractionComponent::ActionRow(row) if row.components.len() == 1 => {
            &row.components[0]
        }
        _ => return Err(InteractionRouteError::UnknownComponent),
    };
    let ModalInteractionComponent::TextInput(input) = input else {
        return Err(InteractionRouteError::UnknownComponent);
    };
    if input.custom_id != "answer"
        || input.value.trim().is_empty()
        || input.value.chars().count() > 4000
    {
        return Err(InteractionRouteError::UnknownComponent);
    }
    Ok(InteractionRoute {
        initial_response: deferred_update(),
        work: Some(RoutedWork::Component(ComponentId::AsyncTextAnswer {
            question_id,
            text: input.value.clone(),
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_window_and_submit_preserve_exact_question_and_multiline_answer() {
        let id = "a".repeat(64);
        let window = serde_json::to_value(open(&id).unwrap()).unwrap();
        assert_eq!(window["type"], 9);
        assert_eq!(
            window["data"]["custom_id"],
            format!("codex_async_reply:{id}")
        );
        let text = "다른 답변\nA=B | C=보류 🙂";
        for layout in [
            json!({"type":18,"id":1,"component":{"type":4,"id":2,"custom_id":"answer","value":text}}),
            json!({"type":1,"id":1,"components":[{"type":4,"id":2,"custom_id":"answer","value":text}]}),
        ] {
            let data: ModalInteractionData = serde_json::from_value(json!({
                "custom_id":format!("codex_async_reply:{id}"),"components":[layout]
            }))
            .unwrap();
            let route = route_modal(&data).unwrap();
            assert_eq!(route.initial_response, deferred_update());
            assert_eq!(
                route.work,
                Some(RoutedWork::Component(ComponentId::AsyncTextAnswer {
                    question_id: id.clone(),
                    text: text.into()
                }))
            );
        }
    }

    #[test]
    fn malformed_or_empty_modal_is_never_an_answer() {
        for (id, field, text) in [
            ("a".repeat(64), "answer", " \n".into()),
            ("a".repeat(64), "wrong", "answer".into()),
            ("invalid".into(), "answer", "answer".into()),
            ("a".repeat(64), "answer", "x".repeat(4001)),
        ] {
            let data: ModalInteractionData = serde_json::from_value(json!({
                "custom_id":format!("codex_async_reply:{id}"),"components":[{
                    "type":18,"id":1,"component":{"type":4,"id":2,"custom_id":field,"value":text}
                }]
            }))
            .unwrap();
            assert!(route_modal(&data).is_err());
        }
    }
}
