# AGY 선택 기능 구현 및 Pro 검수 기록

- 승인 계획: `agy-optional-lifecycle-plan.md`, SHA-256 `3a5d73040aa20e7cd8bba904251902a1a171a2c1c5ca8dfac2bc86e27f478c96`. 계획 본문을 변경하지 않았다.
- 요청: 구현 후 실제 ChatGPT Pro가 계획 준수를 검수한다. 실배포·5060 통합·실제 AGY 로그인/인증 probe/모델/Discord 쓰기는 보류한다.
- 개발: `C:/repos/simdorei/cdr-agy-discord-20261028`, branch `feat/agy-discord-category-20261028`, base `2aaddf87ee037d3ae6dfb5d9d87489430d6ce74e`.
- 변경 전 사본: `D:/codex-work/cdr-agy-target-20261028/implementation-20261029/before/manifest.json`.
- 운영 PID 16212, `C:/repos/simdorei/codex-discord-remote/target/release/cdr-runtime.exe`, 시작 `2026-10-27 14:11:11 +09:00`. 교체/재시작하지 않았다.
- 현재 단계: **로컬 구현 및 실제 ChatGPT Pro R3 검수 PASS**. 전체 runtime·회귀·성능·개발 빌드 완료. 운영 배포/실제 AGY·5060 통합은 요청대로 보류했다.

## 구현

| 계획 | 코드 및 결과 |
|---|---|
| A | `agy/lifecycle.rs`, `manager/activation.rs`: 기본 Off, 명시적 상태·generation·취소. 인증/접수 경계 증명 전 VerificationRequired |
| B | `agy/ownership{,/disk}.rs`, `store.rs`, `rooms.rs`: 읽기 전용 소유권, 생성 receipt별 추적, ID 보존, AGY/충돌/불명확 우선 보호 |
| C | `agy/blocking.rs`, `database.rs`, `manager.rs`, `worker.rs`, `processes.rs`: 단일 DB 작업자, durable 접수와 Off 직렬화, drain/즉시 중단, process/task/DB/handler/permit 정리 전 On 금지 |
| D | `agy/routing.rs`, `routing/interaction.rs`, 공통 ingress/history, gateway topology: 메모리 분기, bounded 독립 ACK/실행/조회/복구 대기열, 공통 fatal 격리. mirror의 모든 삭제·retirement 경계 보호 |
| E | `agy/control{,/ledger}.rs`, `configuration.rs`, windows-native `local_control.rs`: OS PID/creation/exe/SID, current-user ACL, 원격 pipe 거부, runtime/root/protocol binding, 중복·만료·포화, canonical 설정 writer lock |
| F | `codex-discord-tray-agy.ps1`: 비동기 상태/명령, AGY 켜기/끄기/즉시 중단/설정/공식 로그인, stale/runtime mismatch 차단, 표시한 Settings.revision으로 저장 |
| G | 가짜 Discord/CLI/인증 자원, 실제 Windows pipe/프로세스 및 SQLite로 장애·경쟁·성능 검사. 실제 서비스 통합과 구분 |

## 운영 활성화 관문

안전한 실제 CLI 인증 확인 형식과 Discord 접수 시작 경계는 아직 운영 실증되지 않았다. production probe는 VerificationRequired를 반환하여 모델/카테고리 생성을 시작하지 않는다. 로그인 창의 종료나 버전 출력만으로 Ready가 되지 않는다. 인증·네트워크·사용량·모델 권한 오류 판정과 계정 변경 후 기존 대화 접근은 이후 실제 통합 관문에서 확인해야 한다. private Verified fixture를 운영 인증 증거로 사용하지 않는다.

## Pro 구현 R1: REVISE → 수정

대화: https://chatgpt.com/c/6ae22af4-0524-83ec-9f05-35d5d528ca29
전체 R1 응답: `D:/codex-work/cdr-agy-target-20261028/implementation-20261029/pro-r1-response.txt`.

| 지적 | 독립 확인 및 채택 | 재현 검사 |
|---|---|---|
| B1 기록 fetch 후 소유권 재검사 누락 | 수용. 실제 DiscordHistoryCycleIo에서 각 claim 전·process 진입 직전에 현재 ownership 검사. 보호 오류는 공통 fatal로 올리지 않고 진척/복구 notice 유지 | 실제 periodic/gap runner의 held fetch 중 카테고리 이동/epoch 무효화, claim 0·실행 0; claim 후 process에서도 차단 |
| B2 과거 stop의 cutoff/generation 우회 | 수용. mutating stop은 원본 ID cutoff 검사, routing은 관찰 generation 일치 검사. 새 Draining stop 허용 | 실제 control queue 지연 뒤 새 generation, history의 old stop, 새 slash stop, stale Off |
| B3 느린 상태/최종 전송이 initial ACK를 점유 | 수용. bounded ACK 64/64, 별도 거부 ACK 64/64. completion 슬롯 예약 후 defer ACK, 느린 최종 edit은 completion lane. 공통 consumer는 bounded Codex staging 후 AGY route | 상태 전송/최종 edit 5초 hold와 control/lookup full 중 slash/button/autocomplete 40개 ACK≤2500ms, overflow 실행 0, Codex 진행 |
| B4 오래된 settings에 새 status revision 결합 | 수용. 설정 화면은 표시한 Settings.revision을 저장 예상 revision으로 고정. settings 없음은 0, status revision 사용 안 함 | 실제 pipe response의 settings clone 직후 다른 Configure를 완료시키는 barrier; 오래된 저장 CAS 거부 및 최신 설정 보존 + PowerShell 계약 |
| B5 DELETE 이후 custody 손실의 잘못된 미전송 설명 | 수용. post-dispatch 실패는 room ID·dispatched=true·원격 success/rejected를 기록. fence/mapping 보존 | 실제 delete future hold→epoch invalidate→Ok/Rejected, DELETE 1·complete 0·retirement 0 |

추가 R2 근거: 실제 submit의 claim 전/commit 후/enqueue 전 Off 경쟁; 결과 저장 SQL 실패와 sibling panic/소유 child를 겹친 실제 worker 및 Manager 재활성화 금지; Off 뒤 늦은 create receipt 보존; 실제 공통 history/interaction consumer의 오류/panic과 fatal notifier 0; 별도 PID의 위조 pipe 서버에 payload 전송 전 거부; anonymous SID/UNC loopback OS 거부; 두 runtime root가 같은 junction 설정 대상으로 경쟁할 때 writer 1.

## 검사 근거

모든 로그와 측정값은 `D:/codex-work/cdr-agy-target-20261028/implementation-20261029/` 아래다.

- R1 이전 전체 runtime: 612 passed, 5 ignored (`runtime-lib.log`). 당시 코드 범위의 결과이며 최신 코드 전체 검사는 별도 진행한다.
- R2 주요 AGY 검사: 40 passed, 1 subprocess fixture ignored (`revise-agy-full.log`). ignored helper는 부모 forged-PID 검사에서 별도 자식 프로세스로 실제 실행한다.
- R2 history ownership 2, post-DELETE custody 1 검사 통과 (`revise-history.log`, `revise-delete.log`).
- 실제 Windows ACL/current SID 및 UNC loopback remote 거부 2 검사 통과 (`revise-native-control.log`); 오류 5 ACCESS_DENIED 확인, 이름 해석 실패를 거부 성공으로 세지 않았다.
- 세 crate all-targets Clippy `-D warnings` 통과 (`revise-clippy-final.log`).
- 변경 Rust 75개 format check 통과 (`format-r2-final.log`). 전체 workspace 형식 검사에는 기존 미변경 `action_executor/new_thread/verification_tests.rs` 실패가 남아 있다 (`format.log`). 관련 없는 파일은 수정하지 않았다.
- R1 기준 성능 `performance-final.json`: baseline/Off/Ready+5초 전송 hold/Faulted 각 5회, 20회 9000 samples, 제한 통과. R2는 변경된 공통 consumer를 포함하여 `performance-r2.json`에 재측정한다.
- 성능 fixture는 실제 gateway/router/interaction lane, 가짜 Codex admission/첫 응답을 사용한다. 실제 외부 서비스 속도 결과는 아니다.
- 초기 90ms ownership 대기가 Windows 타이머 오차로 시작 차이 101.8874ms를 만들었던 실패는 실제 대기를 75ms로 줄여 해결했다. 100ms 합격선은 유지했고 5초 저장 정지 재검사 통과.
- cold DB fixture를 동시에 실행했을 때 250ms 실패가 있었다. production DB 제한을 늘리지 않았으며 최종 타이밍은 디스크 경합과 구분해 단일 test thread로 실행한다.
- 최종 전체 runtime/회귀/개발 빌드 로그: `runtime-r2-final.log`, `regressions-r2-final.log`, `build-r2-final.log` (진행 중).

## 고정 소스와 남은 검수

R1 snapshot은 `agy-implementation-review-snapshot.json`으로 보존했다. R2 소스 82개 hash는 `agy-implementation-review-r2.json`, manifest SHA-256 `827a816693a669f6f540aa73f9cabbcc77eb101d5610bdde9cf713ebbb878ec8`이다. 이후 Pro 검수 중 소스는 변경하지 않고 이 진행 문서와 결과 기록만 갱신한다.

Pro 연결 화면에서 현재 composer 이름을 놓치는 helper 오류도 기존 승인 범위에서 수정했다. 메인 작업공간의 helper/무결성 pin/회귀 검사에 한정하며 AGY 개발 변경과 구분했다. 관련 Python 73개 통과 후 exact OAuth connector + Pro verified를 확인했다. 실행 중 Rust bridge는 교체하지 않았다.

### R2 최종 검사 중 발견 (Pro R2 소스 고정 유지 중)

- 전체 runtime: 627 passed, 0 failed, 6 ignored, 381.35초. `runtime-r2-final.log`.
- 최신 성능 20회/9000 samples 통과: max admit 0.4913ms, ACK/stop 0.687ms, first 0.4954ms. 최대 p95 증가 0.5244ms 이하.
- 후속 회귀에서 Windows PowerShell 5.1이 `[ulong]` 형식을 인식하지 못해 새 tray 검사 하나가 실패했다. 실제 메뉴에도 같은 표기가 있어 수정 대상이다. `regressions-r2-final.log`. 최종 전체 성공으로 보고하지 않는다.
- 현재 Pro가 읽는 R2 소스는 변경하지 않았다. `[System.UInt64]`를 사용하는 별도 후보는 증거 디렉터리의 `tray-powershell51-fixed.ps1`에서 먼저 검증하고, Pro R2 응답을 받은 뒤 추가 발견과 함께 다음 고정판에 반영한다.
- 같은 Pro 대화에서도 MCP active device selection이 없어 후속 검수가 처음에는 시작되지 않았다. 동일 OAuth 연결 및 동일 PC/root ticket으로 한 번 재선택하여 소스 접근을 복구했다. 다른 연결/PC를 사용하지 않았다.

## Pro 구현 R2: REVISE → R3 수정

완료 응답은 `pro-r2-response.txt`에 보존했다. B1 history, B2 cutoff/generation, B4 revision 경합은 해소로 검수됐다. 남은 세 사항은 독립적으로 확인하고 모두 수용했다.

- Tray: `[System.UInt64]`로 Windows PowerShell 5.1 호환. revision 검증은 form 할당/편집 flag 변경 전에 수행한다. 실제 `powershell.exe`에서 메뉴 5종 mutation, expected revision, 잘못된 설정 후 상태 조회 재개, stale/reordered/runtime mismatch, 비동기 실패 계약 3개 통과 (`r3-tray.log`).
- 빠른 ancestry Err와 panic: AGY fault 기록은 유지하면서 Uncertain로 실행을 금지하고 타입에 맞는 initial ACK를 보낸다. 실제 common consumer에서 slash/button/autocomplete 각 ACK 1회·2500ms 이내, durable claim/enqueue 0, 후속 Codex 진행을 검사했다. 실제 common fatal notifier에 연결된 검사도 3개 입력의 ACK와 오류 격리를 확인한다 (`r3-response.log`, `r3-common-fatal.log`).
- DELETE complete 이후: 실제 caller에 CleanupOutcome을 전달하여 이번 DELETE 성공 / 이전 확정 cleanup 재개 / 이번 원격 부재 관찰을 구분한다. `cleanup`, `archived_rejections`, `orphan_cleanup` 및 exact confirmed 경로의 실제 retirement 경계에서 topology 무효화. 7가지 실행에서 deleted fence와 mapping 보존·retirement 0·추가 DELETE 0 및 정확한 결과 문구를 확인했다 (`r3-retirement.log`).
- 추가 독립 확인: 공통 interaction consumer의 Codex staging 64개가 차면 AGY 분류까지 중단되던 경계를 수정했다. routing은 계속하며 넘친 Codex 입력은 일반/예약 consumer 각각 별도 ACK 전용 최대 64개(동시 합계 128개)에서 명시적으로 거부한다. 이 경로는 DB/claim/실행을 수행하지 않는다. ACK 용량마저 소진되면 기존 gateway와 같이 명시적인 hard-drop 로그를 남기며 다음 AGY 입력 분류를 막지 않는다. 일반 16/예약 4개 실행 상한과 staging 64개는 유지한다. 실제 두 consumer의 Codex 실행·staging을 채운 동안 AGY 3종 ACK≤2500ms, 실행 0 검사가 통과했다.

고정 R3 manifest는 `agy-implementation-review-r3.json`, SHA-256 `104c9cacd12e78f94135029a43d8bda6d36db39e037b35379d3187bea41c9e4a`이다. 소스 84개, R2 대비 17개 변경. R2 실제 소스도 evidence의 `r2-before/`에 보존했다. R3 소스는 검수 중 수정하지 않는다.

- `r3-clippy-final.log`: 세 crate all-targets Clippy `-D warnings` 통과. 첫 Clippy의 test-only hook 및 test cast 경고를 수정한 뒤 재검사했다.
- `format-r3-final.log`: 변경 Rust 77개 format check 통과. `git diff --check` 통과.
- `runtime-r3-final.log`: 전체 runtime 검사 진행 중. `performance-r3.json`은 동일 고정 수치/20회 조건으로 새 공통 루프를 재측정한다.
- 이어서 `regressions-r3-final.log` 및 `build-r3-final.log`에 영향 범위 회귀와 개발 빌드를 기록한다. 완료 로그가 나오기 전 성공으로 세지 않는다.
- 운영 PID 16212 및 시작 시각/실행 경로는 그대로다. 실제 서비스·5060 통합은 하지 않았다.

### R3 최종 실행 근거 갱신

- 전체 runtime 검사 완료: `runtime-r3-final.log`, **630 passed, 0 failed, 6 ignored**, 384.39초. ignored helper의 부모 실행 및 기존 ignored 5개 구분은 R2와 동일하다.
- `performance-r3.json`: 20회/9000 samples, 원래 한도 전부 통과. `performance-r3-summary.json`에 회차별 baseline 대조를 저장했다. 최대 admit 1.1640ms, ACK/stop 1.1526ms, first 1.1645ms. 최대 p95 증가 admit 0.0666ms, ACK 0.0790ms, stop 1.0232ms, first 0.0659ms. fake 측정이며 실서비스 수치가 아니다.
- `regressions-r3-final.log`와 개발 빌드는 이어서 진행 중이다. 검수 대상 소스 84개는 변경하지 않았다.

- R3 후속 회귀 완료: history adapter 4, mirror 91, startup 5, history wiring 11, AGY tray 3, 기존 tray reliability 14/revision3 10, worker supervision 4 모두 통과 (`regressions-r3-final.log`).
- R3 개발 실행 파일 빌드 완료: `D:\codex-work\cdr-agy-target-20261028\debug\cdr-runtime.exe`, SHA-256 `9b1ba87181cca756326585b884255c0519c8dab61c306d9c28bf054f68ef3879` (`build-r3-final.log`). 실행/배포하지 않았다. 전체 근거 요약 `verification-r3.json`, 소스 hash 84개 재확인. Pro 최종 판정은 아직 대기 중이다.

## 최종 Pro R3: PASS

[실제 검수 대화](https://chatgpt.com/c/6ae22af4-0524-83ec-9f05-35d5d528ca29). `ask-chatgpt-pro` 스킬로 같은 Chrome Pro 대화와 지정 OAuth connector / sim-pc-3060 / 개발 폴더를 사용했다. Pro는 실제 소스·R2 사본·R3 manifest·완료 로그·성능 원자료를 읽어 대조했다. 원문은 evidence의 `pro-r3-response.txt`, 최종 증거 요약은 `verification-r3.json`에 보존했다.

Pro의 완료 판정은 **“PASS — 고정 R3 소스의 로컬 구현·계약 검증 범위”**다. R1의 history/generation 지적, R2의 tray/빠른 lookup ACK/삭제 완료 후 결과 표시 지적은 모두 해소됐다. Codex staging 포화 처리도 수용됐으며 추가 필수 수정 요청은 없다. Codex는 각 지적을 실제 코드와 대조한 뒤 필요한 수정만 채택했고, 원래 합격선을 낮추지 않았다.

범위와 남은 검증:

- production은 안전한 실제 인증 조회와 입력 접수 시작 경계의 운영 증명 전까지 `VerificationRequired`다. fake Verified 성공을 운영 Ready로 인정하지 않는다. 실제 AGY 로그인·probe·모델, Discord 변경, 5060 통합은 실행하지 않았다.
- 성능은 로컬 fake Codex/Discord 조건의 계약 결과다. 실제 네트워크 속도나 GUI·pipe 전체 연결 통합 성공을 주장하지 않는다. native pipe/OS 신원 및 PowerShell helper는 각 별도 검사에서 검증했다.
- overflow ACK 제한은 consumer별 64개다. 새 포화 검사는 Codex 실행/staging을 정지시키며 rejection 자체는 즉시 완료한다. ACK 64개 모두를 hold한 뒤 최종 hard-drop까지의 경계는 소스로 검토했고 별도 재현 검사는 하지 않았다. Pro는 구체적인 필수 결함을 발견하지 않아 PASS의 추가 조건으로 삼지 않았다.
- 변경 Rust 77개 format check 통과. 전체 workspace에서 기존 미수정 `action_executor/new_thread/verification_tests.rs` 형식 문제는 남아 있으며 관련 없는 코드를 바꾸지 않았다.
- 개발 실행 파일은 D 드라이브의 격리 target에 만들었고 실행하지 않았다. 운영 PID 16212 / 시작 2026-10-27 14:11:11.7595737 +09:00 / 기존 release 경로는 그대로다.
- 구현은 `feat/agy-discord-category-20261028` 로컬 작업 브랜치에 있다. 이 작업에서 commit/push/merge 또는 배포는 하지 않았다.
