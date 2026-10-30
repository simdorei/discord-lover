# 기존 Codex Rust 브릿지의 교체 설치

`update.ps1`은 실행 중인 Codex 전용 Rust 브릿지를 같은 폴더에서 Discord Lover로
교체한다. 기존 `.env`를 다시 만들거나 봇·서버·채널 식별자를 변경하지 않는다.
CODEX_HOME, 대화 기록, 첨부파일, 플러그인 등록, Windows 자동시작 작업은 이동하거나
재등록하지 않는다. AGY 설정이 없거나 Off인 설치만 대상으로 한다.

## 실행

기존 설치 폴더 밖에 이 저장소를 내려받는다. 설치 대상은 정상 실행 중이며
heartbeat와 identity-bound drain handshake를 지원하는 Rust 버전이어야 한다.
Python/TS 전환, 정지 상태의 신규 설치, 알 수 없는 DB 버전은 이 명령의 대상이 아니다.
Rust 1.97.1과 Windows C++ 빌드 도구는 신규 설치와 동일하게 필요하다.

```powershell
cd C:\repos\discord-lover
powershell -NoProfile -ExecutionPolicy Bypass -File .\update.ps1 -TargetRoot "C:\기존\브릿지폴더"
```

빌드는 새 저장소에서 먼저 끝낸다. 빌드 실패는 기존 봇에 영향을 주지 않는다.
이미 만든 실행 파일이 있다면 다음과 같이 지정한다.

```powershell
.\update.ps1 -TargetRoot "C:\기존\브릿지폴더" -CandidatePath "D:\build\cdr-runtime.exe" -SkipBuild
```

`-Preview`는 준비된 실행 파일로 호환성·설정·현재 실행 상태만 검사한다. 백업,
작업 접수 차단, 종료, 교체, 재시작은 실행하지 않는다. 제어 잠금 파일의 소유 정보는
동시 점검을 막기 위해 기록할 수 있다. 모든 코드·DB 쓰기가 없는 완전한 파일 무변경
모드라는 뜻은 아니다.

```powershell
.\update.ps1 -TargetRoot "C:\기존\브릿지폴더" -CandidatePath "D:\build\cdr-runtime.exe" -SkipBuild -Preview
```

## 보존과 완료 기준

- `.env`의 원래 바이트와 접근 권한을 보존하고 별도 백업한다.
- 실제 설정된 DB 경로를 사용한다. 외부 경로와 SQLite WAL에 남은 커밋도
  SQLite backup API로 포함한다. 종료 전·후 두 시점의 일관된 백업을 남긴다.
- 교체 파일은 `target/release/cdr-runtime.exe`, `codex-discord-tray.ps1`,
  `codex-discord-tray-agy.ps1`로 제한한다. 소스·사용자 데이터 폴더 전체를 덮지 않는다.
- 기존 실행 호환성 보호 장치가 있으면 정책과 지원 형식은 유지하면서 실행 파일
  식별값만 새 버전에 맞춘다. 새 버전이 기존 보류·복구 기록을 읽을 수 있는지
  검사한 후에만 교체한다. 기존 보호 파일을 지워서 시작하지 않는다.
- 실제 실행 파일 SHA-256, 프로세스 경로·PID·시작 시각, 서로 다른 두 heartbeat를
  확인한 뒤 완료를 기록한다. 같은 파일의 재실행은 다시 재시작하지 않는다.
- AGY 로그인, 모델 호출, 카테고리 생성, Discord 알림 전송은 설치 도구가 하지 않는다.

기존 트레이 창이 이미 열려 있으면 그 창은 기존 코드로 계속 표시될 수 있다.
교체 후 AGY 메뉴가 필요할 때 기존 트레이의 **Exit tray icon**을 선택하고 기존
폴더의 `codex-discord-tray.ps1`을 다시 실행한다. 봇을 중지할 필요는 없다.

## 중단과 복구

진행 상태는 기존 폴더의 `.codex_discord_rust.upgrade.json`에 기록된다.
실행 파일·설정 백업과 완료 영수증은 `.codex-discord-backups/upgrade-<operation>/`에
남는다. DB 백업은 실제 DB 옆 `.codex-discord-backups/`에 저장된다.
백업에는 개인 설정이 있으므로 Git에 추가하지 않는다.

작업 대기 제한(`-WaitTimeoutSeconds`, 기본 900초)에 도달하면 강제로 종료하지
않는다. 기존 프로세스가 새 작업 접수를 막은 채 대기 중일 수 있다. 잠금·대기 표식을
임의로 지우지 말고 동일한 버전의 업데이트 도구로 이어서 실행한다.

```powershell
.\update.ps1 -TargetRoot "C:\기존\브릿지폴더" -Resume -WaitTimeoutSeconds 1800
```

종료 후 복사나 사전 시작 검사가 실패하면, 새 봇을 한 번도 실행하지 않았고
알 수 없는 보조 프로세스가 없다는 조건에서 이전 파일과 보호 설정을 복구해
기존 버전을 다시 시작한다. 이 경우 `upgrade_rolled_back`을 출력하며 실패를
성공으로 보고하지 않는다. **DB를 과거 백업으로 자동 복원하지 않는다.**

새 봇의 시작을 시도한 뒤에는 메시지를 처리했을 가능성이 있다. 시작 결과가
불명확하거나 프로세스가 사라졌다면 기록과 정지 보호 상태를 유지한다. `-Resume`은
기록된 살아 있는 자식 프로세스만 확인하며, 봇을 무조건 다시 띄우거나 옛 DB로
돌리지 않는다. 결과가 불명확한 보조 명령도 자동 재실행하지 않는다.

원래 업데이터 코드, 실행 파일 백업, 설정 또는 다른 작업의 표식이 변경되면
자동 진행을 거부한다. 이 경우 기록된 오류와 프로세스 상태를 확인해야 한다.
`.env`가 같아도 재개한 터미널의 상속 환경이 달라지면 같은 방식으로 보류한다.
실제 DB 경로·CODEX_HOME·Discord 설정·AGY 설정 선택을 포함한 유효 환경을
재개 직후와 후보 또는 이전 프로그램을 시작하기 직전에 다시 검사한다.
설정이 불일치하면 자동 복구를 통해 이전 프로그램을 시작하지도 않는다.
원래 실행 환경을 복구한 뒤 동일한 도구로 `-Resume`해야 한다.

## 검증 범위

`windows_upgrade_contract`는 실제 Windows 테스트 프로세스와 임시 설치 폴더를
사용하여 교체·작업 대기·업데이터 중단 후 재개·시작 실패·이전 파일 복구를 검사한다.
`upgrade_inspection_contract`는 실제 Rust 관리 명령과 SQLite DB로 외부 DB 경로,
WAL 백업, 설정 보존, 미래 DB 형식 거부를 검사한다. 실제 Discord/API/AGY에는
연결하지 않는다. 운영 PC와 5060 통합 적용은 별도 검증이며 이 테스트에 포함되지 않는다.

2026-10-29 로컬 검증에서는 위 두 테스트와 `admin_cli_contract`,
`installer_staging_contract`, `maintenance_native_contract`, `restart_native_contract`,
`windows_agy_tray_contract`, `windows_watchdog_contract`를 함께 실행해
69개 검사가 통과했다. 변경 코드와 새 테스트의 Clippy 경고 없음 검사,
설치 dry-run, 전체 Rust 형식 검사와 workspace 빌드도 통과했다.
전체 workspace 테스트 실행은 도중에 중단하고 위 관련 회귀 검사로 검증 범위를
한정했으므로 전체 테스트 통과를 의미하지 않는다.

2026-10-30에는 최신 `main`의 질문 답변·busy 수정과 합친 뒤 위 검사에
`async_recovery_compatibility_contract`를 더해 75개 검사를 통과했다. 이후
현재 질문 답변 저장 형식의 호환성과 재개 전 `.env` 변경 보호를 추가하여
교체 관련 두 테스트를 다시 실행했고 13개가 통과했다. 중복을 제외한 관련
검사는 총 77개다. 최신 변경 후 `cargo clippy --workspace --all-targets --locked -- -D warnings`
검사도 통과했다. 이 결과는 앞의 2026-10-29 검증과 구분한다.

교체 성공 검사는 `.env` 원문, 외부 CODEX_HOME의 대화 파일, Discord 프로젝트·
스레드 연결과 읽기 위치가 보존되는지도 확인한다. 재개 전에 사용자가 `.env`를
수정하면 그 변경을 덮어쓰지 않고 교체를 중단한다. 새 질문 답변 형식은 허용하되
지원하지 않는 미래 형식은 DB 백업이나 프로세스 정지 전에 거부한다.

같은 날 Pro 원문 검수에서 상속 환경이 달라진 재개·복구 경로의 검증 누락이
지적됐다. 수정 전 4개 회귀 검사가 실제로 실패했으며, 재개 직후·시작 직전·
자동 복구 직전의 동일 설정 검사를 추가했다. `draining`, `stopped`, `installed`,
`rolling_back`, `launch_ready`, `launching/prepared` 중단 지점에서 변경된 DB·
CODEX_HOME·AGY 설정은 보류하고, 원래 환경으로 재개하면 한 번만 시작하는지
확인했다. 테스트 중 발견한 다른 프로세스의 종료 경합도 별도로 재현했다.
종료가 확인된 프로세스만 제외하며 확인할 수 없는 살아 있는 프로세스는 계속 거부한다.

수정 후 `upgrade_inspection_contract`, `windows_upgrade_contract`,
`windows_upgrade_resume_contract`, `windows_upgrade_process_contract`의 18개
검사가 통과했다. 앞의 관련 검사와 중복을 제외하면 총 82개이며, 최신 변경에
대한 전체 workspace/all-target Clippy 경고 없음 검사도 통과했다. 새 중단 지점
시험은 임시 복사본에만 대기 구문을 넣어 실제 단계 저장 직후 업데이터를 종료한다.
제품에는 시험용 우회 경로를 추가하지 않았다.

검증 중 확인한 Windows 특이점은 다음과 같다.

- PowerShell 7에서 상속한 모듈 검색 경로 때문에 Windows PowerShell이 다른 버전의
  ACL 모듈을 불러올 수 있어, 실행 중인 셸의 모듈을 명시적으로 사용한다.
- heartbeat 파일의 원자적 교체 순간에도 읽을 수 있도록 쓰기·삭제 공유를 허용한다.
  PID·시작 시각·신선도·두 번의 시각 증가 확인은 그대로 유지한다.
- 테스트 출력은 파일로 수집한다. 살아 있는 Windows 자식 프로세스가 파이프 핸들을
  상속해 업데이트가 끝난 뒤에도 테스트 출력 수집을 막을 수 있기 때문이다.
