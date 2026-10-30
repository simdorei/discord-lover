# Discord Lover

Discord에서 Codex와 선택형 AGY를 연결하는 **Rust 브릿지 개발 저장소**입니다.
Codex+AGY 개발본 전체와 기존 Rust 저장소의 Git 이력을 이어받았습니다.

AGY는 기본 비활성이며, 실제 인증 확인과 메시지 접수 경계 검증이 끝나기 전에는
`VerificationRequired`에서 멈춥니다. 현재 개발본은 실제 AGY 작업이나 카테고리
생성을 시작하지 않습니다. 로컬 구현과 Pro R3 검수는 완료됐지만 실제 서비스
통합 검증은 남아 있습니다. [이전 및 검증 범위](docs/discord-lover-import.md)를 확인하세요.

설치·보조 도구도 Rust를 사용하며 Python 설치는 필요하지 않습니다.
기존에 실행 중인 봇에 자동 반영되는 것은 아닙니다.
[전환 범위와 개발 당시 검증 기록](docs/python-free-migration.md)을 확인하세요.

기반 저장소는 [codex-discord-remote-rust](https://github.com/simdorei/codex-discord-remote-rust)입니다.
해당 Rust 저장소의 이력을 보존했으며 실제 대화 기록, 사용자 설정,
데이터베이스 및 실행 파일은 포함하지 않습니다.

## 포함된 기능

- Codex app-server를 통한 요청 처리, 대기열 및 진행 중 요청 수정(steering)
- Codex 스레드와 Discord 스레드 연결, 메시지 미러링, 최종 답변 전달
- 새 스레드, 보관, 모델 설정, 사용량, 진단 및 도움말 명령
- Windows 트레이, 상태 확인, 재시작 및 자동 실행 보조 도구
- 선택 기능인 ChatGPT Pro 연결, 원격 MCP 서버와 PC 에이전트
- 선택 기능인 [AGY CLI 전용 카테고리와 대화 연결](docs/agy-discord.md) (기본 비활성)
- 재시작·메시지 전달·저장 상태를 검증하는 자동 테스트

실제 명령 목록은 Discord의 `!help` 또는 `/help`를 확인하세요.
`!new` 다음에 보내는 메시지는 새 Codex 스레드의 첫 요청입니다.

진행 중인 작업 때문에 재시작이 대기 상태라면 Discord에서 `!restart_codex force` 또는 `!force_restart`를 보내세요. Windows에서는
`codex-discord-force-restart.cmd`를 실행하거나 트레이의 **Force restart bot + app-server (interrupt work)**를 선택하세요.
작업·승인·종료 대기를 건너뛰고 봇과 소유 앱서버를 강제로 재시작합니다.
[명령과 복구 동작](docs/force-restart.md)을 참고하세요.

## 실행 구조

`crates/`에 Rust 작업 공간이 있으며, `cdr-runtime`이 Discord 봇 실행 파일입니다.
저장 상태는 SQLite, Codex 연결은 app-server가 담당합니다.

봇뿐 아니라 설치·설정, 플러그인 검사, 첨부파일 전송, 저장 상태 진단과
Pro 보조 도구도 Rust 실행 파일을 사용합니다. PowerShell·셸은 운영체제 실행과
예약 작업 연결을, JavaScript는 Chrome 화면 제어를 담당합니다.
Python 설치 파일·소스·전용 복구 경로는 제거했습니다. 오류가 나면 실제 실패를
표시하며, 다른 언어의 실행본으로 자동 전환하지 않습니다.

## Windows 설치

필요한 항목:

- Git, 로그인된 Codex 및 app-server를 제공하는 Codex 실행 파일
- `rust-toolchain.toml`에 고정된 Rust 1.97.1
- Rust Windows 빌드에 필요한 Visual Studio C++ Build Tools와 Windows SDK
- 직접 만든 Discord 봇과 본인 서버의 관리 권한

```powershell
git clone https://github.com/simdorei/discord-lover.git
cd discord-lover
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\setup-discord-bot.ps1
```

`install.ps1`이 소스를 빌드해 `target\release\cdr-runtime.exe`까지 만듭니다.
별도로 `cargo build`를 실행할 필요는 없습니다. 실행 파일 제작·설치까지만
하려면 `setup-discord-bot.ps1`은 아직 실행하지 마세요. 이 두 번째 단계는
토큰을 저장하고 자동 실행을 등록한 뒤 실제 봇을 시작합니다.

첫 번째 단계가 끝나면 다음 명령으로 실행 파일을 확인할 수 있습니다.
도움말만 출력하며 Discord에는 연결하지 않습니다.

```powershell
.\target\release\cdr-runtime.exe --help
```

기존 Python판을 사용하던 PC는 신규 설치 명령을 그대로 실행하기 전에
[Python판에서 Rust판으로 전환하기](docs/windows-python-to-rust.md)를 따르세요.
기존 설정·대화방 연결 정보를 보존하고, 구버전과 신버전의 중복 실행을 막는
순서가 별도로 필요합니다. Python 자체를 먼저 삭제할 필요는 없습니다.

설치 도구는 Rust 봇과 Rust Pro 도우미를 빌드하고 플러그인을 확인합니다.
설정 도구는 로컬 `.env`를 만들고 Windows 자동 실행 작업을 등록합니다.
기존 봇이 있는 PC에서는 설정 도구를 중복 실행하지 마세요.
같은 Discord 봇 토큰으로 두 실행본을 동시에 켜지 마세요.
이미 설치된 실행 파일과 다른 버전이면 설치 도구가 덮어쓰기를 거부합니다.
기존 Codex 전용 Rust 설치를 교체하려면 **별도 Discord Lover 폴더**에서 실행하세요.
기존 폴더의 `.env`, DB, 첨부파일, 봇 연결 정보와 자동시작 등록은 유지합니다.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\update.ps1 -TargetRoot "C:\기존\브릿지폴더"
```

새 실행 파일을 빌드한 뒤 작업 완료 대기 → 정상 종료 → 백업 → 교체 → 재시작을
진행합니다. 기본 대기 시간은 15분이며 강제로 작업을 끊지 않습니다. AGY는 꺼진
상태로 유지합니다. [교체 설치·검증·실패 복구](docs/windows-safe-upgrade.md)를 확인하세요.

설치 전에 변경 없이 확인하려면:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -DryRun -SkipDependencies -SkipEnvFile
```

`.env.example`을 참고해 허용할 Discord 서버·사용자·채널을 좁게 설정하세요.
토큰은 `.env`에만 보관하며 Git에 올리지 않습니다.
Windows의 Codex 앱 설치 폴더나 `.sandbox-bin`에서 실행기를 선택하면 같은 폴더의
`codex-code-mode-host.exe`도 확인합니다. 파일이 빠졌다면 `CODEX_EXE`에 고정된
설치라도 건너뛰고 정상 설치를 자동 검색합니다. 앱 설치의 실행 파일 수정 시각이
최신인 순서로 확인한 뒤 `.sandbox-bin`, PATH 순서로 탐색하며, 불완전한 설치는
제외합니다. 정상 후보가 하나도 없을 때만 누락 경로를 표시하고 시작을 중단합니다.
이 과정에서 `.env`를 바꾸거나 다른 설치의 host 파일을 복사하지 않습니다.
정상인 `CODEX_EXE` 지정은 우선하며, 지정한 실행 파일 자체가 없는 경우에는 기존처럼
오류를 표시합니다. 값을 비워 두면 처음부터 자동 검색하므로 특정 PC의 경로나
업데이트마다 바뀌는 버전 폴더를 고정할 필요가 없습니다.
MCP 예제의 `mcp.example.com`은 실제 주소가 아닙니다. 선택 기능을 사용할 때만
본인이 관리하는 서버 주소와 기기별 인증 값을 설정하세요.

## 직접 빌드 및 실행

```powershell
cargo build --release --locked -p cdr-runtime
.\target\release\cdr-runtime.exe --help
.\target\release\cdr-runtime.exe --env .\.env --check-config
.\target\release\cdr-runtime.exe --env .\.env
```

설치 후에는 `codex-discord-bot.cmd`로 콘솔에서 봇을 실행할 수 있습니다.
설정 도구로 자동 실행을 켰다면 이 명령으로 두 번째 봇을 시작하지 마세요.
상태 확인은 다음 명령으로 실행합니다. 이 명령은 봇을 시작하거나 멈추지 않습니다.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\codex-discord-rust-status.ps1
```

이 저장소 공개 작업은 기존 설치의 업데이트나 재시작을 수행하지 않습니다.
배포·복구 스크립트는 실제 실행 중인 봇에 영향을 줄 수 있으므로
내용과 대상 폴더를 확인한 뒤 사용하세요.

## 테스트

일반 자동 테스트는 가짜 서버와 임시 데이터로 실행합니다.
실제 계정·기기를 사용하는 `ignored` 테스트는 자동으로 실행하지 않습니다.
테스트에는 Rust, Git, PowerShell, Node.js 24가 필요하며 Windows에서는 Git Bash도
사용합니다. Python 실행기나 패키지는 필요하지 않습니다. 과거 동작과의 비교에는
실행하지 않는 고정 JSON·SQL 자료와 Rust로 작성한 가짜 서버를 사용합니다.
[Windows 자동 검사 설정](.github/workflows/windows-contract.yml)을 참고하세요.
백업 패키징 검사는 대상 저장소의 봇이 꺼져 있는지 확인합니다.
다른 저장소에서 실행 중인 봇을 해당 검사 대상과 혼동하지 않습니다.
이 저장소의 테스트를 위해 기존 봇을 자동으로 정지하지 않습니다.

```powershell
cargo test --workspace --locked
powershell -NoProfile -ExecutionPolicy Bypass -File .\plugins\codex-discord-remote\scripts\qa-smoke.ps1
```

`Cargo.lock`과 `rust-toolchain.toml`을 함께 유지해 빌드 버전을 고정합니다.
실제 Discord 송수신 확인은 자동 테스트와 별개이며, 본인 테스트 채널에서 진행하세요.

## 선택 기능: Rust MCP 서버

`cdr-mcp-server`와 `cdr-remote-agent`가 서버 및 PC 측 코드를 제공합니다.
서버 배포 예시는 `remote_mcp_server/Dockerfile.rust`와 `compose.rust.yaml`입니다.
기본 `Dockerfile`·`compose.yaml`도 같은 Rust 서버를 사용합니다.
이 공개 작업에서는 VPS나 기존 MCP 서버를 변경하지 않습니다.
인증 설정과 HTTPS 프록시는 본인 환경에 맞게 별도로 구성해야 합니다.

기기 두 대 중 한 대의 재시작과 OAuth 인증을 로컬에서 확인하려면:

```powershell
cargo test --locked -p cdr-mcp-server --test multi_device_restart_smoke_contract --test remote_agent_e2e_contract --test oauth_http_contract --test mcp_http_contract
```

이 검사는 임시 서버만 사용합니다. 실제 VPS 연결 상태를 보증하는 검사는 아닙니다.

## 범위와 제한

- Windows 트레이와 운영 스크립트가 주 설치 경로입니다.
- macOS/Linux에 대한 동일 기능 보장을 의미하지 않습니다.
- Computer Use의 화면 캡처 가능 여부는 Codex 도구와 운영체제 지원에 달려 있습니다.
- 개인 작업 기록, 배포 영수증, 로그, DB, 첨부파일 및 비밀 값은 공개하지 않습니다.
- Rust의 정비 예제와 `scripts/Register-CdrMaintenance.ps1`은 가짜 경로·식별자를 사용합니다.
  실행 가능한 개인 배포 지시서가 아니며, 별도 검토 없이 실행하지 마세요.
- 개인 배포 인증 기록은 가져오지 않아 `drain-certified-artifacts.json`은 빈 목록입니다.
  검증된 종료 절차를 요구하는 관리형 배포는 새 환경의 검증 기록이 없으면 거부됩니다.
- Pro 연결에는 기존 `Simdorei Local Project Oauth` 이름·주소 계약이 남아 있습니다.
  임의의 MCP 주소만 바꾼다고 Pro 커넥터까지 자동 변경되지는 않습니다.
- `NOTICE.md`와 각 플러그인의 고지문에 포함된 출처 표시를 유지합니다.

### Manual Reserve

Use `!settings --model gpt-reserve` to select Reserve explicitly. The bridge validates capacity and the applied model, effort and speed. It never switches into or out of Reserve automatically. Retired `--auto-reserve` / slash `auto_reserve` requests are refused as a whole, including old bound actions.

A usage-limit rejection is held and never replayed after a model change or restart. Send a new request after changing settings. `!runners` lists execution holds. Startup retires old policy authority before queue recovery, retaining audit evidence and unaccepted legacy input. Exact saved-final recovery is a local admin operation described in [manual Reserve and recovery](docs/manual-reserve.md); it does not run the original turn again.
