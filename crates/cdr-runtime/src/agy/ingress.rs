use super::{AgyError, AgyRuntime, Result, Room};
use std::{fmt::Write, sync::Arc};
use twilight_model::{
    application::interaction::{
        Interaction, InteractionData, InteractionType, application_command::CommandOptionValue,
    },
    channel::Message,
    http::interaction::InteractionResponse,
};

const HELP: &str = "AGY 전용 채널입니다. 프로젝트 채널에 질문하면 새 대화가 생기고, 대화 안에서는 이어서 답합니다.\n!new 질문 · !ask 질문 · !stop · !status\n/new · /ask · /stop · /status 도 사용할 수 있습니다. Codex sync·모델 설정은 Codex 채널에서 실행해 주세요.";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Action {
    Prompt { prompt: String, new: bool },
    Stop,
    Status,
    Help,
    Reject(&'static str),
}

fn parse(text: &str) -> Action {
    let text = text.trim();
    if text.is_empty() {
        return Action::Reject("질문을 함께 입력해 주세요.");
    }
    if !text.starts_with('!') {
        return Action::Prompt {
            prompt: text.into(),
            new: false,
        };
    }
    let (command, body) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let body = body.trim();
    match command {
        "!new" | "!ask" if !body.is_empty() => Action::Prompt {
            prompt: body.into(),
            new: command == "!new",
        },
        "!new" | "!ask" => {
            Action::Reject("명령 뒤에 질문을 입력해 주세요. 예: !new 이 프로젝트를 살펴봐")
        }
        "!stop" if body.is_empty() => Action::Stop,
        "!status" | "!where" if body.is_empty() => Action::Status,
        "!help" if body.is_empty() => Action::Help,
        _ => Action::Reject("AGY에서 지원하지 않는 명령입니다. !help 로 사용법을 확인해 주세요."),
    }
}

impl AgyRuntime {
    pub(crate) async fn report_message_failure(
        &self,
        message: &Message,
        error: &AgyError,
    ) -> Result<()> {
        self.messenger.send(message.channel_id.get(),&format!("message:{}:error",message.id),
            &format!("AGY 메시지 처리 중 오류가 발생했습니다: {error}\n실행 여부는 !status 로 확인해 주세요.")).await
    }
    pub(crate) async fn handle_message(
        self: &Arc<Self>,
        message: &Message,
        bot: Option<u64>,
    ) -> Result<bool> {
        if message.author.bot
            || !self.policy.enable_message_content
            || message.webhook_id.is_some()
            || message.guild_id.map(twilight_model::id::Id::get) != Some(self.guild)
            || !self.user_allowed(Some(message.author.id.get()))
        {
            return Ok(false);
        }
        let Some(room) = self
            .rooms
            .resolve(
                message.channel_id.get(),
                self.guild,
                &self.remote,
                &self.mirror_db,
                &self.ownership,
                &self.store,
            )
            .await?
        else {
            return Ok(false);
        };
        let text = strip_bot_mention(&message.content, bot);
        let action = if message.attachments.is_empty() {
            parse(text)
        } else {
            Action::Reject(
                "AGY 첨부파일 전달은 아직 지원하지 않습니다. 작업 폴더의 파일 경로를 질문에 적어 주세요.",
            )
        };
        let id = format!("message:{}", message.id);
        let response = self.act(&id, message.channel_id.get(), room, action).await;
        let text = match response {
            Ok(text) => text,
            Err(error) => format!("AGY: {error}"),
        };
        if !text.is_empty() {
            self.messenger
                .send(message.channel_id.get(), &format!("{id}:receipt"), &text)
                .await?;
        }
        Ok(true)
    }

    #[allow(deprecated, reason = "older Discord interactions use channel_id")]
    pub(super) async fn complete_interaction(
        &self,
        event: &Interaction,
        action: Action,
        interactions: &dyn super::transport::Interactions,
    ) -> Result<()> {
        let channel = event
            .channel
            .as_ref()
            .map(|c| c.id)
            .or(event.channel_id)
            .ok_or_else(|| AgyError::Invalid("AGY interaction lacks room".into()))?;
        let room = self
            .rooms
            .resolve(
                channel.get(),
                self.guild,
                &self.remote,
                &self.mirror_db,
                &self.ownership,
                &self.store,
            )
            .await?
            .unwrap_or(Room::Unsupported);
        let id = format!("interaction:{}", event.id);
        let response = match self.act(&id, channel.get(), room, action).await {
            Ok(text) => text,
            Err(error) => format!("AGY: {error}"),
        };
        let response =
            cdr_discord::text::fit_single_message(&response, cdr_discord::text::DISCORD_MAX_LEN);
        interactions
            .update(
                event,
                if response.is_empty() {
                    "AGY 요청을 접수했습니다."
                } else {
                    &response
                },
            )
            .await
    }

    async fn act(&self, id: &str, channel: u64, room: Room, action: Action) -> Result<String> {
        if action == Action::Stop {
            let input_id = id
                .rsplit(':')
                .next()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| AgyError::Invalid("Invalid AGY control identity".into()))?;
            if input_id <= self.acceptance.lock().await.cutoff {
                return Err(AgyError::Invalid(
                    "활성화 이전 중단 요청은 현재 작업에 적용하지 않습니다.".into(),
                ));
            }
        }
        if room == Room::Unsupported {
            return Ok(
                "AGY 설정에 등록되지 않은 채널입니다. 등록된 AGY 프로젝트 채널을 사용해 주세요."
                    .into(),
            );
        }
        if let Action::Prompt { prompt, new } = action {
            return self
                .submit(id, channel, room, prompt, new)
                .await
                .map(|message| {
                    if message == "AGY 요청을 접수했습니다." {
                        String::new()
                    } else {
                        message.into()
                    }
                });
        }
        let _permit = self.admission.try_enter_control()?;
        if !self.store.claim(id, channel).await? {
            return Ok(String::new());
        }
        let response = match action {
            Action::Stop => {
                let active = self.active()?;
                let job = active
                    .values()
                    .find(|a| a.origin == channel || a.target == Some(channel));
                if let Some(job) = job {
                    let _ = job.stop.send(true);
                    "AGY 중단을 요청했습니다.".into()
                } else {
                    "실행 중인 AGY 작업이 없습니다.".into()
                }
            }
            Action::Status => self.status(channel).await?,
            Action::Help => HELP.into(),
            Action::Reject(message) => message.into(),
            Action::Prompt { .. } => unreachable!(),
        };
        self.store.finish(id, "control", &response).await?;
        Ok(response)
    }

    async fn status(&self, channel: u64) -> Result<String> {
        let running = self
            .active()?
            .values()
            .any(|a| a.origin == channel || a.target == Some(channel));
        let mut text = if let Some(session) = self.store.session(channel).await? {
            let mut text = format!(
                "AGY · {}\n대화: {}\n작업 폴더: {}",
                if running { "실행 중" } else { "대기 중" },
                session
                    .conversation
                    .as_deref()
                    .unwrap_or("아직 시작하지 않음"),
                session.cwd.display()
            );
            if session.blocked {
                text.push_str("\n이전 실행의 대화 번호를 확인할 수 없어 이 대화의 재실행을 보류했습니다. !new 질문 을 사용해 주세요.");
            }
            text
        } else {
            format!(
                "AGY 프로젝트 채널 · {}",
                if running {
                    "실행 중"
                } else {
                    "질문을 보내면 새 대화를 만듭니다."
                }
            )
        };
        if let Some((state, result)) = self.store.last_result(channel).await? {
            let _ = write!(text, "\n마지막 상태: {state}");
            if matches!(
                state.as_str(),
                "failed" | "interrupted" | "delivery_failed" | "stopped"
            ) {
                let _ = write!(text, "\n{result}");
            }
        }
        Ok(text)
    }
}

pub(super) async fn acknowledge(
    interactions: &dyn super::transport::Interactions,
    event: &Interaction,
    response: &InteractionResponse,
    deadline: tokio::time::Instant,
) -> Result<()> {
    if tokio::time::Instant::now() >= deadline {
        return Err(AgyError::Discord(
            "AGY acknowledgment expired; command not executed".into(),
        ));
    }
    tokio::time::timeout_at(deadline, interactions.acknowledge(event, response))
        .await
        .map_err(|_| {
            AgyError::Discord("AGY acknowledgment deadline exceeded; command not executed".into())
        })?
        .map_err(|_| AgyError::Discord("AGY acknowledgment failed; command not executed".into()))?;
    Ok(())
}

fn strip_bot_mention(text: &str, bot: Option<u64>) -> &str {
    let text = text.trim();
    if let Some(bot) = bot {
        for mention in [format!("<@{bot}>"), format!("<@!{bot}>")] {
            if let Some(rest) = text.strip_prefix(&mention) {
                return rest.trim();
            }
        }
    }
    text
}

pub(super) fn slash_action(event: &Interaction) -> Action {
    let Some(InteractionData::ApplicationCommand(data)) = &event.data else {
        return Action::Reject("이 버튼은 AGY 대화에서 사용할 수 없습니다.");
    };
    if event.kind != InteractionType::ApplicationCommand {
        return Action::Reject("AGY 명령 형식이 올바르지 않습니다.");
    }
    if data.options.iter().any(|option| option.name != "prompt") {
        return Action::Reject(
            "AGY 명령은 다른 대화 지정이나 Codex 설정 옵션을 지원하지 않습니다.",
        );
    }
    match data.name.as_str() {
        "new" | "ask" => {
            let prompt = data.options.iter().find_map(|o| {
                if let CommandOptionValue::String(value) = &o.value {
                    Some(value.trim())
                } else {
                    None
                }
            });
            prompt.filter(|s| !s.is_empty()).map_or(
                Action::Reject("질문을 입력해 주세요."),
                |prompt| Action::Prompt {
                    prompt: prompt.into(),
                    new: data.name == "new",
                },
            )
        }
        "stop" => Action::Stop,
        "status" | "where" => Action::Status,
        "help" => Action::Help,
        _ => Action::Reject("이 명령은 AGY에서 지원하지 않습니다. Codex 채널에서 실행해 주세요."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, options: &serde_json::Value) -> Interaction {
        serde_json::from_value(serde_json::json!({
            "id":"1","application_id":"2","type":2,"token":"local-test","version":1,
            "authorizing_integration_owners":{},"entitlements":[],
            "channel_id":"3","guild_id":"4","user":{"id":"7","username":"tester","discriminator":"0001","avatar":null},
            "data":{"id":"5","name":name,"type":1,"options":options}
        })).unwrap()
    }

    #[test]
    fn slash_commands_preserve_prompt_and_reject_codex_options() {
        assert_eq!(
            slash_action(&command(
                "new",
                &serde_json::json!([{"name":"prompt","type":3,"value":"한글 질문"}])
            )),
            Action::Prompt {
                prompt: "한글 질문".into(),
                new: true
            }
        );
        assert_eq!(
            slash_action(&command("stop", &serde_json::json!([]))),
            Action::Stop
        );
        assert!(matches!(
            slash_action(&command("bridge_sync", &serde_json::json!([]))),
            Action::Reject(_)
        ));
        assert!(matches!(
            slash_action(&command(
                "status",
                &serde_json::json!([{"name":"ref","type":3,"value":"codex-id"}])
            )),
            Action::Reject(_)
        ));
        assert!(matches!(parse("!new"), Action::Reject(_)));
        assert_eq!(strip_bot_mention("<@!42> !ask test", Some(42)), "!ask test");
    }
}
