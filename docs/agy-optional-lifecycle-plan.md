# AGY 선택 기능 분리: 설계·구현 계획

문서 버전: R2, 2026-10-28. 이 파일은 검수 중 고정한다. 판정·검수 기록은 `agy-optional-lifecycle-review.md`에 별도로 기록한다. 구현·배포 승인이나 통과 판정이 아니다.

## 목표와 고정 범위

- 기존 Rust Discord 봇 하나에서 Codex와 AGY를 사용한다. AGY는 기본 꺼짐이다.
- Codex 카테고리와 별개인 AGY 카테고리를 사용한다. 기존 서버·봇 토큰·허용 사용자 설정을 보존한다.
- AGY의 설치 확인, 설정, 로그인 안내, 활성화, 중단, 오류 복구를 Windows 트레이에서 관리한다.
- AGY를 켜고 끄거나 AGY 초기화가 실패해도 Codex app-server와 정상 대화가 재시작되거나 중단되면 안 된다.
- 개발 대상은 `feat/agy-discord-category-20261028`, 기준 커밋은 `2aaddf87ee037d3ae6dfb5d9d87489430d6ce74e`이다. 기존 미커밋 개발 변경을 보존한다.
- 현재 허용 범위는 설계·구현 계획의 검수다. 실행 중인 3060 브릿지 수정, 5060 봇 교체·재시작, 실제 Discord 방 생성, AGY 로그인·모델 실행, 실제 통합 테스트는 하지 않는다.
- 추후 구현은 로컬 모의 검사와 빌드까지 구분하고, 5060 실제 적용은 별도 단계로 남긴다. 기존 단독 AGY 봇의 자동 중지·세션 이관은 이번 구현에 포함하지 않는다.

## 현재 코드에서 확인된 문제

1. `discord_runtime.rs`의 AGY 설정 읽기와 `AgyRuntime::prepare()` 오류가 `?`로 전체 시작에 전파된다. 카테고리 준비도 공통 시작 경로에서 기다린다.
2. AGY worker가 필수 worker의 종료 통지 채널을 공유한다. AGY worker 종료가 전체 브릿지 종료로 이어질 수 있다.
3. ingress와 history 경로는 `Option<Arc<AgyRuntime>>`에 의존한다. AGY가 없으면 기존 AGY 방이 Codex 처리로 넘어갈 여지가 있다.
4. `Store::open()`은 스키마 생성과 중단 요청 복구를 수행한다. 방 소유권만 확인하려고 호출해서는 안 된다.
5. 트레이는 5초 간격으로 봇 상태를 관찰하며 현재 재시작·로그 열기 기능만 있다. UI 관찰 실패를 실행 권한으로 취급하지 않는 기존 규칙을 보존해야 한다.
6. CLI 실행기는 공식 print/stream-json, 대화 번호 저장, 요청 중복 방지, 소유한 프로세스 트리 중단을 이미 구현했다. 이를 유지한다.

## 작업 구조와 책임

### 1. 공통 Discord 연결과 항상 존재하는 방 소유권 확인

- Discord gateway와 bot token은 하나만 유지한다.
- AGY 실행 상태와 독립적인 `AgyOwnership`을 공통 ingress에 둔다. 최소 상태는 `NeverConfigured`, `Ready(snapshot)`, `Unavailable(reason)`이다.
- 저장된 category/project/session ID를 AGY DB에서 읽기 전용으로 불러온다. 설정 파일이나 CLI가 없어도 기존 방 소유권을 잊지 않는다.
- 처음부터 AGY를 사용한 적 없는 설치는 빈 소유권 상태로 동작한다. 기존 AGY DB가 있는데 읽기에 실패한 경우를 빈 DB와 동일하게 처리하지 않는다.
- 메시지, 슬래시 명령, 버튼, 자동완성, gateway gap 복구, 주기 history polling 모두 같은 분류 결과를 사용한다.
- 분류 결과는 `Codex`, `Agy`, `Unmanaged`, `Uncertain`, `Conflict`로 구분한다. AGY 비활성·인증 필요·오류 상태에서도 `Agy`가 `Codex`로 바뀌지 않는다.
- 알려진 Codex/AGY ID는 메모리에서 판정한다. 모르는 방만 제한된 비동기 조회로 부모 관계를 확인하며 실패를 Codex 허용으로 바꾸지 않는다.
- 방 이동·삭제·스레드 생성 gateway 이벤트에서 분류 캐시를 무효화한다. 이벤트 누락 후 재연결에서는 다시 검증한다. 이름만으로 기존 방을 인수하지 않는다.
- 소유권 읽기 실패 시 확인된 Codex 방의 일반 작업은 계속한다. 분류가 불명확한 방은 실행하지 않고 오류를 표시한다.
- Codex mirror sync의 삭제 단계에도 소유권 확인을 적용한다. AGY 소유·충돌·분류 불명확 대상은 삭제하지 않는다. 소유권을 확인할 수 없으면 해당 정리 단계를 보류하고 이유를 보고한다.
- AGY를 꺼도 카테고리·스레드·기록·소유권 정보는 삭제하지 않는다.

### 2. 독립 AGY 실행 관리

- `AgyManager`가 설정·활성화 시도·인증 확인·실행 worker·중단을 소유한다. Discord 연결 자체는 공통 계층에 둔다.
- 같은 Rust 프로세스 내에서 별도 작업과 대기열을 둔다. 실제 CLI는 기존처럼 별도 자식 프로세스다. OS 수준의 전체 프로세스 장애 격리를 보장한다고 표현하지 않는다.
- AGY worker의 오류·panic·종료는 관리자가 받아 AGY 오류 상태로 전환한다. 필수 Codex worker 종료 통지로 전달하지 않는다.
- 시작 시 최소 소유권 상태를 확립한 뒤 Codex ingress를 열고, AGY CLI 확인·인증·Discord 방 준비는 별도로 진행한다.
- 각 활성화 시도에 generation을 부여한다. 끄기·설정 변경·새 활성화 후 도착한 이전 결과는 상태를 다시 켤 수 없다.
- 외부 방 생성이 이미 진행된 상태에서 취소돼도 확인된 생성 결과와 소유권은 기록한다. 결과가 불명확한 생성 요청은 자동으로 재실행하지 않는다.
- AGY 작업은 별도의 접수 게이트와 한도 안에서 실행하며 기존 전체 재시작용 AdmissionPermit도 유지한다.
- 저장 실패는 성공으로 처리하지 않는다. 실패 사유와 요청 결과의 확실성을 보존한다.

### 3. 트레이와 로컬 제어

- `codex-discord-tray.ps1`에는 메뉴 연결만 추가하고, AGY UI·로컬 통신은 별도 helper 파일로 나눈다.
- AGY 메뉴: 상태, 켜기, 로그인, 실행 파일·프로젝트 설정, 다시 확인, 끄기, 오류 내용 보기.
- 작업 중에는 `작업 완료 후 끄기`와 명시적인 `지금 중단 후 끄기`를 구분한다. 트레이 종료는 기존처럼 아이콘만 종료한다.
- Windows 로컬 named pipe로 고정된 명령만 전달한다. 임의 shell 문자열 실행 API를 만들지 않는다.
- pipe는 현재 Windows 사용자 범위로 제한하고 원격 클라이언트를 거부한다. 관리자 권한 상승을 요구하지 않는다.
- handshake에 프로토콜 버전·정규화한 브릿지 경로·현재 프로세스 시작 신원·runtime UUID를 포함한다. PID 재사용이나 다른 설치본으로 명령이 가면 거부한다.
- 명령에 request ID와 예상 설정 revision을 포함한다. 중복 요청은 같은 처리 결과를 반환하고, 충돌한 설정 저장은 덮어쓰지 않는다.
- 설정·상태의 최종 작성자는 runtime 하나다. 트레이는 전송 성공과 실제 적용 완료를 구분한다.
- UI 이벤트에서 CLI나 네트워크 완료를 기다리지 않는다. 5초 타이머는 캐시된 상태만 읽고, 무응답·오래된 상태는 `확인 불가`로 표시한다.
- 브릿지가 꺼져 있으면 활성화 요청을 뒤늦게 재생할 파일로 남기지 않는다. 브릿지를 먼저 시작해야 한다고 안내한다.

## R2 확정 계약 1: 공유 입력에서 AGY를 분리하는 위치

다음은 선택적인 개선이 아니라 B·C·D 단계의 필수 완료 조건이다.

| 경로 | 분기 위치와 허용 작업 |
|---|---|
| 일반·긴급 메시지 | `typed_ingress/message.rs`에서 `handle_message_create`를 기다리기 전에 공통 접근 정책과 메모리 분류를 수행한다. AGY는 제한된 전용 큐에 `try_send`하고 즉시 공통 소비자로 돌아온다. |
| 슬래시·버튼·자동완성 | `typed_ingress/interaction.rs`의 normal/reserved future 집합에 넣기 전에 분류한다. AGY가 Codex의 normal 16개/reserved 4개 슬롯을 점유하지 않는다. |
| history polling | `history/cycle.rs`는 메모리 분류만 사용한다. AGY/Conflict/Uncertain 대상은 Codex polling에서 제외하고 필요한 작업을 전용 복구 큐에 전달한다. |
| gateway gap | `history/gap.rs`는 AGY 복구를 기다리지 않는다. AGY 복구자가 revision을 확인해 자기 gap만 완료한다. 실패한 gap은 보존하고 Codex gap 처리를 계속한다. |
| 미분류 방 | 별도의 제한된 분류 큐에서만 Discord 조회를 한다. 공통 슬롯·루프를 잡지 않으며, 명확히 Codex/Unmanaged로 확인된 요청만 기존 접근 정책을 다시 확인한 뒤 Codex에 전달한다. |

- 공유 계층의 AGY 관련 동작은 immutable snapshot 조회와 비차단 전달로 제한한다. DB·네트워크·응답 전송·큐 여유 대기는 이 경계 안에서 하지 않는다.
- AGY normal/control/recovery 큐를 분리하고 한도를 둔다. stop/status·Discord interaction ACK는 AGY 자체의 우선 처리 경로를 사용한다. 큐가 가득 찬 요청은 접수 완료로 표시하지 않는다. 상태 안내 실패도 Codex 오류로 전파하지 않는다.
- 공통 접근 정책(서버·사용자·bot/webhook·message-content 허용)은 우회하지 않는다. 미분류 요청도 실행 전에 동일 정책과 현재 lifecycle을 재검증한다.
- AGY 소유권 조회, 요청 처리, 상태 안내, history 복구, 실행·전달 작업의 오류와 panic은 AGY 경계에서 처리한다. 해당 작업의 JoinHandle 또는 제한된 AGY adapter의 unwind 경계에서 수집하며, 공통 worker의 `?` 또는 fatal notifier로 전파하지 않는다. Codex 자신의 오류 처리까지 삼키지 않는다.
- 동기 SQLite와 파일 작업은 AGY 전용 DB 작업자에서 수행한다. async 함수에서 동기 mutex/SQLite를 그대로 호출하지 않는다. 작업자는 최대 1개이며, 요청 큐도 제한한다.
- 소유권 초기 읽기 대기는 최대 100ms, 개별 DB 작업 응답 대기는 최대 250ms로 제한한다. SQLite busy timeout도 작업 예산 이내로 설정한다. 기한 초과는 Unavailable/Faulted로 게시하고 Codex를 계속 시작한다.
- 타임아웃은 동기 I/O가 끝났다는 증거가 아니다. 멈춘 DB 작업자가 있으면 대체 작업자를 계속 만들지 않으며, 완료를 확인하기 전 새 DB writer·Store recovery를 시작하지 않는다.
- 이 격리는 Rust task panic과 일반 오류 범위다. 프로세스 abort·OS 종료·전체 메모리 고갈까지 별도 서비스처럼 격리됐다고 주장하지 않는다.

## R2 확정 계약 2: 접수·중단·재활성화의 순서

- On은 Off 또는 정리가 끝난 SetupRequired/AuthRequired/VerificationRequired/Faulted 상태에서만 허용한다. Checking/Provisioning의 중복 On은 현재 시도만 반환한다.
- Draining 또는 FaultedCleaning에서는 On, 활성화 목적 Recheck, 로그인 새 시도, 설정 변경을 거절한다. 자동 재활성화 예약을 남기지 않는다. 상태 조회와 소유 작업의 명시적 중단만 제공한다.
- FaultedCleaning은 오류 후 정리 중이며, FaultedBlocked는 정리 완료를 입증할 수 없는 상태다. 단순히 오류 메시지를 표시했다는 이유로 새로운 worker를 시작하지 않는다.
- 다음 generation의 worker와 Store recovery는 이전 generation의 신규 접수 폐쇄, 모든 접수 작업의 terminal/uncertain 분류, 소유 자식 프로세스 트리 종료 확인, 관련 task 회수, active 등록·admission permit 해제가 모두 확인된 뒤에만 시작한다.
- 정상 drain에는 자동 강제 종료 시간을 두지 않는다. 명시적 즉시 중단 또는 worker 장애로 필요한 종료는 owned Job Object에만 수행한다. 종료 요청 후 5초 내 확인되지 않으면 FaultedBlocked로 남기고, 다른 프로세스를 이름으로 찾아 죽이지 않는다.
- 결과 저장 실패와 무관하게 프로세스·task·permit 정리를 수행한다. 저장 실패 때문에 cleanup을 조기 반환하는 코드를 허용하지 않는다. DB에 결과를 확정하지 못한 요청은 uncertain으로 취급하고 자동 재실행하지 않는다.
- Store recovery는 부팅 또는 위 정리 조건이 성립한 관리자 전환에서만 한 번 수행한다. 조회·트레이 상태 갱신·소유권 snapshot 갱신에서는 호출하지 않는다.

### 접수의 단일 확정 시점

1. 관리자 하나가 Submit과 Off를 직렬 처리한다. 현재 generation/Ready/접수 경계를 확인하고 AGY 및 전체 drain permit, bounded worker 큐 슬롯을 먼저 예약한다.
2. 작업 등록 준비 후 request ID와 generation을 durable accepted 상태로 commit하는 시점을 접수 확정으로 정한다. 이 짧은 commit 동안 Off 처리 순서는 보존되며 DB 대기는 위 250ms 한도를 따른다. UI는 Off 적용 응답 전까지 `끄는 중`으로 표시한다.
3. commit 전에 실패하면 예약·permit을 해제하고 실행하지 않는다. commit 후에는 예약된 큐에 동기 전달한다. 전달 불능·worker 종료·crash가 발생하면 accepted-but-interrupted/uncertain으로 남기고 자동 재실행하지 않는다. worker의 durable running 전환 이전에는 CLI를 시작하지 않는다.
4. Off가 처리된 경계 이전의 accepted 작업은 정상 drain 대상이다. 이후 Submit은 accepted로 commit하지 않는다. `접수 중`과 `접수 완료`를 구분해 알린다.
5. 이전 generation의 대화 ID·결과·확인된 방 생성 결과는 해당 요청 기록과 ownership에 보존한다. 현재 lifecycle·설정 revision을 덮어쓰는 권한은 주지 않는다.

### Ready 게시와 과거 입력 배제

- 새 generation의 epoch/cutoff와 알려진 방의 입력 경계를 먼저 확정·영속화하고, 그 성공 후에만 Ready 및 접수 개방을 하나의 관리자 상태로 게시한다. 저장 실패 시 접수하지 않는다.
- message-create는 원래 메시지 ID/생성 시점을, interaction은 새 interaction ID/생성 시점을 기준으로 한다. 오래된 메시지의 버튼을 지금 누른 경우 원래 메시지의 나이가 아니라 새 interaction을 판정한다.
- gateway에서 받은 시각만으로 과거 메시지를 새 요청으로 바꾸지 않는다. gap/history와 지연 전달도 같은 경계를 적용한다. Off/Checking 기간에 이미 거절한 ID는 새 generation에서 재접수하지 않는다.
- cutoff 취득과 READY 게시 사이의 race, 시계 오차, gateway 재연결을 다루는 구체 알고리즘과 경계 fixture를 A 단계에서 고정한다. 원격 watermark/수신 epoch로 과거 입력 배제를 입증하지 못한 채 로컬 시계만 믿고 Ready를 열지 않는다. 해당 증명이 안 된 방은 접수 보류한다.

## R2 확정 계약 3: 소유권 게시와 모든 정리 부작용의 보호

### gateway 전달 및 snapshot

- 변경 대상에 `cdr-discord/src/gateway/ingress/publication.rs`, ingress 이벤트 타입·큐, `gateway_identity.rs`와 연결 재개/손실 신호를 추가한다. 현재 MessageCreate/InteractionCreate 외의 이벤트가 Ignored인 상태를 그대로 두지 않는다.
- ChannelCreate/Update/Delete, ThreadCreate/Update/Delete/ListSync 및 READY/RESUMED·새 연결 epoch·큐 overflow/이벤트 손실을 runtime의 topology 경로로 전달한다. topology 경로는 prompt 큐와 분리한다.
- snapshot은 guild/root, version, gateway epoch, 알려진 AGY IDs와 부모 관계, 검증 상태를 가진다. 이벤트 누락·재연결 시 오래된 부정 판정은 Uncertain으로 만들고 제한된 background 검증으로 갱신한다.
- 판단 우선순위는 Conflict → 명시적 AGY 소유/AGY 부모 → 현재 검증된 Codex 소유 → 검증된 Unmanaged이다. 단순히 Codex mapping이 먼저 발견됐다는 이유로 AGY 증거를 건너뛰지 않는다.
- `확인된 Codex`는 현재 증거에서 충돌이 없는 대상이다. stale mapping만으로 판정하지 않는다. 소유권 손상 시 이러한 확인된 대상의 실행은 계속하고, 증명이 없는 대상만 차단한다. 모든 메타데이터를 잃은 방까지 무조건 정상 처리한다고 주장하지 않는다.
- 최초 AGY 외부 생성 전에 root에 `ever_configured` 표식을 영속화한다. AGY DB가 사라졌거나 설정 경로가 없어져도 과거 사용 이력을 NeverConfigured로 오인하지 않는다. 표식이나 DB를 읽을 수 없으면 빈 소유권으로 대체하지 않는다.
- 생성/인수의 주체인 관리자가 확인된 category/project/thread ID를 durable ownership에 기록하고 snapshot에 게시한 뒤 그 방을 실행 가능하게 만든다. 기록 실패 시 메모리 보호를 먼저 유지하고 재시작 후에는 불완전 상태로 차단한다.
- 생성 요청이 진행 중이거나 결과가 불명확하면 그 범위의 미분류 입력을 Codex로 전달하지 않는다. 취소 후 도착한 확정 ID도 게시한다. 생성 결과 기록 작업은 Ready 전환 취소와 별개로 회수한다. 설정 제거가 기존 ID 삭제를 의미하지 않는다.

### 변경 경계별 보호

| 소스 | 보호할 부작용 |
|---|---|
| `mirror_sync/cleanup.rs` | stale thread DELETE, `retire_thread_sync` |
| `mirror_sync/orphan_cleanup.rs` | orphan DELETE, stale project DELETE, `retire_project_sync` |
| `mirror_sync/archived_rejections.rs` | 직접 `dispatch_guarded_delete`, 원격 부재 시 cleanup complete, mapping retirement |
| `mirror_sync/delete_guard.rs` | cleanup begin/complete, `fence_missing_room`, 실제 DELETE dispatch |
| `mirror_sync/exact_cleanup.rs` | 원격 삭제 없는 mapping retirement 및 runtime 외부 exact-maintenance 진입점 |

- AGY/Conflict/Uncertain는 위 부작용 전체에서 제외한다. DELETE 0회만 확인하고 로컬 mapping이나 cleanup 상태를 변경하는 구현은 불합격이다.
- 정리 시작에 version이 붙은 ownership permit을 받고, 실제 원격 dispatch 및 로컬 cleanup 확정·retirement 직전에도 유효성을 확인한다. 로컬 소유권 등록과 정리 대상 예약은 같은 조정 경계를 통과하며 한 대상에 동시에 소유권 인수와 정리가 진행되지 않게 한다.
- 삭제를 전송하지 않은 경우, 전송 후 결과가 불명확한 경우, 원격 부재가 확인된 경우를 기존 fence 규칙대로 구분한다. 소유권 재검증 실패를 `deleted` 또는 `DeleteRejected`로 바꾸지 않는다. 이미 전송한 삭제는 취소됐다고 주장하지 않고 uncertain fence와 ID를 보존한다.
- 이 가드는 기존 pending-work 보호를 대체하지 않는다. 두 조건을 모두 만족해야 한다. 외부에서 Discord 구조를 바꾸는 행위와 원격 DELETE를 원자적으로 묶을 수 있다는 보장은 하지 않는다.
- runtime 밖의 exact cleanup은 기존 배타 실행 소유권을 확인하고 같은 root의 read-only ownership을 주입한다. AGY 이력이 있는 root에서 guard 미설정/읽기 실패를 기본 허용으로 대체하지 않는다.

## R2 구현 관문: 제어 통신과 설정 작성자

- pipe 서버/클라이언트가 주장하는 PID·UUID를 자기신고만으로 신뢰하지 않는다. OS의 pipe peer process ID, 프로세스 시작 신원·실행 경로, 기존 runtime identity를 연결해 확인한다. 서버는 연결 사용자의 실제 SID를 확인하며, impersonation을 쓰면 반드시 원래 신원으로 복귀한다.
- 현재 사용자 DACL과 원격 pipe 거부를 설정하고 로컬 fixture로 검증한다. 클라이언트도 다른 프로세스가 같은 handshake를 흉내 낸 서버를 거부해야 한다.
- mutating request ID는 runtime UUID, 불변 발급 시각, nonce를 포함한다. 발급 시각은 동일 PC의 단조 시계 기준으로 검증하며 최초 접수 유효기간은 30초다. payload hash·예상 revision을 함께 기록한다.
- 동일 ID/동일 payload는 Pending 또는 기존 결과를 반환하고 재실행하지 않는다. 동일 ID/다른 payload는 거절한다. 발급 시각을 바꿔 같은 ID를 재사용할 수 없도록 ID와 시각의 일치를 검증한다.
- active 작업과 완료 후 최소 10분의 결과를 보관한다. 최대 4096개이며 유효한 기록을 밀어내지 않는다. 포화 시 신규 변경 요청을 명시적으로 거절하고 읽기 전용 상태 조회는 유지한다. 만료된 요청은 실행하지 않으며, 결과 조회만 허용한다. 보관된 결과가 없어지면 `결과 만료—상태 확인 필요`를 반환한다.
- runtime UUID 변경 후 이전 명령을 자동 재전송하지 않는다. 승인된 과거 명령의 응답은 현재 lifecycle 상태와 다르다. 트레이는 runtime UUID·설정 revision·상태 sequence를 검사해 역순/늦은 응답으로 이전 Ready를 복원하지 않는다.
- 설정 작성자 잠금은 root 잠금과 별개로 canonical 설정 경로에 건다. Windows 경로의 대소문자·별칭·reparse 해석 후 실제 대상 경로를 사용하고, 파일 생성 전이면 실제 부모 경로와 파일 이름을 사용한다. 해당 설정의 writer lock은 runtime 수명 동안 보유한다.
- 다른 root/session이 같은 설정을 가리켜도 writer lock을 동시에 얻을 수 없어야 한다. 잠금 충돌은 AGY 설정 오류로 격리한다. 설정을 저장하는 트레이 우회 경로나 두 번째 writer를 만들지 않는다.
- E 단계에서 OS 신원·권한·중복 보관·설정 잠금이 위 계약을 만족하지 않으면 제어 기능 완료로 표시하지 않는다. 인증·Discord 호출 없이 검사할 수 있다.

## 설정과 상태 전환

- 설정 파일 경로의 존재와 활성화 여부를 분리한다. 새 설정의 `enabled` 기본값은 false다.
- 기존 개발용 `DISCORD_AGY_CONFIG`는 설정 위치로 유지하되, 경로가 있다는 이유만으로 기능을 켜지 않는다. 아직 배포하지 않은 개발 기능의 의미 변경임을 문서에 명시한다.
- 설정은 version/revision을 포함하고 임시 파일 작성 후 원자적으로 교체한다. 손상·지원하지 않는 버전은 AGY 설정 오류로 표시한다.
- 실행 파일, 프로젝트 이름·폴더, 기존 category/channel ID를 검증한다. 이미 연결된 대화의 폴더나 ID를 다른 대상으로 조용히 바꾸지 않는다.
- 자격 증명은 설정에 저장하지 않는다. 자동 승인 기본값 false와 기존 허용 사용자 정책을 유지한다.
- 저장된 사용 의도와 실제 상태를 구분한다. `enabled=true`는 사용 요청이며 `Ready`의 증거가 아니다.

| 현재/사건 | 다음 상태와 동작 |
|---|---|
| 기본 상태 | Off. CLI와 인증 조회를 시작하지 않음 |
| 허용된 상태에서 켜기 | 이전 실행 정리 완료 확인 후 사용 의도 저장, generation 증가, Checking 시작 |
| 설정/CLI 문제 | SetupRequired. 원인과 설정 메뉴 제공 |
| 명시적인 인증 부재 | AuthRequired. 신규 요청과 방 생성 금지 |
| 인증 판정 불가 | VerificationRequired. 성공·로그아웃으로 추정하지 않음 |
| 인증 확인 후 | Provisioning. 기존 방 검증 또는 필요한 방 생성 |
| 준비 완료 | 현재 generation 확인 후 Ready 및 요청 접수 허용 |
| 초기 설정 중 취소 | 사용 의도 false, 시도 취소, Off |
| Ready에서 끄기 | 사용 의도 false 저장, 새 접수 차단, Draining |
| 접수된 작업 모두 종료 | Off. 소유권·대화 기록 유지 |
| 즉시 중단 선택 | 소유한 AGY 작업 프로세스만 중단·회수 후 Off |
| 사용 중 인증 오류 | AuthRequired. 진행 결과를 저장하고 자동 재전송 금지 |
| 네트워크·사용량 제한 | 별도 일시 오류. 인증을 지우거나 로그인 창을 띄우지 않음 |
| worker 비정상 종료 | FaultedCleaning. Codex 유지, 소유 작업 정리 후에만 재활성화 확인 제공 |
| 정리 미확인 | FaultedBlocked. On/설정 변경 금지, 상태 확인·소유 작업 중단만 제공 |
| 브릿지 재시작 | 저장된 의도에 따라 확인을 재개하되 로그인 창 자동 실행 금지 |

- Draining 중에는 접수된 작업만 완료한다. 시간이 오래 걸린다는 이유로 자동 강제 종료하지 않는다. 사용자에게 남은 작업과 즉시 중단 선택을 보여 준다.
- 끄기를 영구 저장하지 못하면 성공으로 표시하지 않는다. 현재 프로세스의 신규 접수는 닫고, 재시작 시 재활성화될 수 있다는 상태를 명시한다.
- 프로젝트·실행 파일·계정 변경은 실행 중 작업에 즉시 적용하지 않는다. 비활성 상태에서 적용하고 다시 검증한다.

## 인증 확인과 로그인

확인 사실: 5060에서 읽기 전용으로 확인한 CLI는 1.4.9이며 `--help`에 전용 auth-status subcommand는 없다. 공식 문서는 interactive `agy`의 기존 인증 공유와, 비대화형 실행의 `authentication required` 오류를 설명한다.

- 공식 CLI가 인증을 소유한다. 토큰·쿠키·Windows 자격 증명 내용을 읽거나 복사하지 않는다.
- CLI 설치 파일 존재, 버전 출력, 로그인 프로세스 종료 코드만으로 인증 성공을 판정하지 않는다.
- 로그인은 사용자가 트레이에서 요청할 때만 공식 interactive CLI를 열어 진행한다. 브릿지와 같은 Windows 사용자·프로필이어야 한다.
- 로그인 UI를 열 수 없는 세션이면 해당 PC의 대화형 세션에서 진행해야 한다고 표시한다. Discord에 인증 URL·코드·토큰을 전송하지 않는다.
- 로그인 창 중복 실행을 막고, 창 종료 후에도 별도 확인 결과가 있어야 Ready가 된다. 취소된 generation의 로그인 성공은 재활성화 권한이 없다.
- 오류 분류는 인증 부재/거부, 자격 증명 저장소 접근 문제, 네트워크, 사용량 제한, 모델 권한, CLI 호환성, 불명확 오류를 구분한다. 모든 비정상 종료를 로그인 만료로 해석하지 않는다.
- 계정 전환·재인증 후에는 기존 대화가 재개되는지 확인하지 않고 새 대화로 대체하지 않는다. 대화 접근 거부 시 해당 대화를 막고 설명한다.

### 구현 전 인증 확인 수단 확정 관문

`-p /usage`의 구조화된 조회는 후보일 뿐, 아직 이 환경에서 안전한 인증 확인 계약이 검증되지 않았다. 공식 변경 기록에는 모델 실행·사용량 소비·대화 생성 없는 조회로 기재되어 있다. 그러나 `disableSlashCommands` 설정에 따라 일반 프롬프트가 될 수 있으므로 단순히 이 문자열을 실행하면 안 된다.

1. 지원 CLI 버전, 실제 인수 문법, slash 처리 강제 방법, 구조화된 성공/실패 결과를 공식 자료와 검증된 fixture로 확정한다.
2. 모델 실행·대화 생성 없는 조회임을 입증할 수 있는 경우에만 그 어댑터를 허용한다. 알려지지 않은 버전·형식은 VerificationRequired로 둔다.
3. probe에 시간·출력·프로세스 한도를 두고, 성공은 기대한 조회 응답과 정상 종료가 모두 있어야 한다. 시간 초과는 로그아웃으로 판정하지 않는다.
4. 최초 켜기·명시적 다시 확인·로그인 완료 확인에서만 조회한다. 매 메시지나 트레이 갱신마다 실행하지 않는다.
5. 이 관문이 해결되지 않으면 인증 자동 판정을 완료했다고 주장하지 않고 기능 활성화/실배포를 보류한다. 실제 AGY 호출로 검증하는 단계는 현재 사용자가 보류한 통합 테스트와 분리해, 추후 허용 범위가 확인된 뒤 수행한다.
6. 조회 JSON/정상 종료가 단순한 캐시·공개 목록이 아니라 실제 인증을 필요로 하는 성공 증거인지 검증한다. 잘못된 Windows 프로필, 인증 없음, 불명확 응답을 성공으로 취급하지 않는다. fixture 검사는 지원 CLI의 안전성과 인증 의미에 대한 실증을 대신하지 않으며, 증명되지 않은 probe 어댑터 자체도 실행하지 않는다.

공식 근거:
- https://antigravity.google/docs/cli/headless/
- https://antigravity.google/docs/install?tab=cli
- https://antigravity.google/docs/changelog?tab=cli (v1.1.11의 print-mode 조회)
- https://antigravity.google/docs/cli/reference/ (`--disable-slash-commands` 및 설정)

## 기록, 비활성 기간, 복구

- AGY Store의 스키마/복구/쓰기와 소유권 읽기를 분리한다. 재활성화할 때 실행 중인 요청을 다시 중단 복구 대상으로 만들지 않는다.
- 접수된 Discord 요청 ID, CLI 대화 ID, 실행 결과와 전달 결과를 구분해 저장한다. 네트워크 오류 후 모델 요청을 자동으로 다시 실행하지 않는다.
- 꺼져 있거나 인증 확인 중인 기간의 메시지는 실행 대기열에 넣지 않는다. 가능하면 상태 안내만 보내고, 나중에 켜도 이를 재생하지 않는다.
- 각 활성화마다 접수 시작 경계와 채널별 high-water mark를 저장한다. 실시간 수신과 gap/history 복구가 같은 경계·중복 방지 규칙을 적용한다.
- Discord 메시지 생성 시점/ID 기준으로 활성화 이전 메시지를 배제한다. 재시작 시 이전 실행 기간을 자동 복구해 모델 작업을 재생하지 않는다.
- 생성 결과 불명확, DB 기록 실패, 중간 종료는 구분해 남긴다. 새 카테고리·새 대화 생성으로 오류를 숨기지 않는다.
- AGY 작업 승인 요청을 Discord에서 답하는 기능은 이번 범위 밖이다. 승인 필요는 로그인 오류와 구분해 안내하며 자동 승인으로 전환하지 않는다.
- 로그에는 단계·오류 분류·상관 ID를 기록하고 자격 증명·인증 코드·전체 프롬프트를 기록하지 않는다.

## 성능 기준

- AGY 미사용/Off 경로: AGY CLI 자식 프로세스 0, 인증 조회 0, AGY transcript 순회 0.
- 알려진 Codex 방의 메시지 분류에는 추가 AGY 네트워크 요청과 동기 DB 조회가 없어야 한다.
- AGY 네트워크 지연·CLI 정지·대기열 포화가 Codex ingress를 기다리게 하지 않는다. 불명확한 방 조회도 공통 수신 루프에서 기다리지 않는다.
- AGY 접수·출력 대기열은 크기를 제한하고 Codex 작업과 별도로 관리한다. 공유 Discord 전송 한도와 PC CPU/메모리까지 완전히 독립적이라고 주장하지 않는다.
- 로컬 fake에서 기존 기준본/AGY Off/AGY Ready+지연/AGY 오류를 같은 입력으로 비교한다. 필수 관문은 추가 호출 수 0, Codex 응답이 AGY 해제 전 완료됨, 대기열 크기 제한 준수다.
- 실서비스 지연 수치와 5060 동시 부하는 추후 통합 검사 항목이며, 로컬 검사 결과로 대체하지 않는다.

### 로컬 합격 수치와 재현 조건

- A 단계에 fixture clock, 입력, 측정 코드를 먼저 고정한다. 동일 PC·동일 빌드 설정·임시 DB·지연 없는 fake Codex/Discord를 사용하고, 통과를 위해 이후 기준을 낮추지 않는다. 기준본 자체가 아래 절대 한도를 넘으면 측정 환경/기존 병목을 구분해 조사하며 성능 통과로 표시하지 않는다.
- 한 실행은 예열 20건 후 Codex 메시지 200건과 interaction 40건(그중 stop 10건)을 5ms 간격으로 공급한다. 기준본/AGY Off/Ready+지연/Faulted를 각각 5회 교차 순서로 실행한다. 입력과 시간은 로그로 남기고 각 회차를 개별 판정한다.
- 스트레스 조건은 AGY 일반 요청 128건으로 전용 큐 포화를 유도하고, AGY 방 조회/전송을 각각 5초 동안 완료시키지 않으며, DB 잠김과 history 복구 오류는 별도 케이스로 준다. 가능한 모든 공통 ingress 경로를 실제 구현 경로로 통과시킨다.
- 측정점: gateway 수신 → Codex 접수, interaction 수신 → ACK, stop 수신 → stop 처리, gateway 수신 → 첫 fake 응답. 단조 시계로 개별 지연과 nearest-rank p95·최댓값을 기록한다.
- 각 회차의 p95 증가는 기준본 대비 `max(5ms, 기준본 p95의 5%)` 이하여야 한다. 절대 한도는 Codex 접수/첫 fake 응답 각각 500ms, stop 250ms, 모든 interaction ACK 2500ms다. AGY를 풀기 전에 끝났다는 사실만으로 통과시키지 않는다.
- 시작 검사에서 AGY 초기 DB 읽기를 5초 정지시켜도, fake 환경의 Codex Ready는 기준본 Ready보다 최대 100ms만 늦을 수 있다. 추가 프로세스·인증 조회·Codex 메시지당 추가 AGY DB/네트워크 호출 수는 기존 0 기준을 유지한다.
- 이 수치는 향후 검사의 사전 합격 기준이며 측정 결과가 아니다. 기계적 fake 검증과 실서비스 체감 지연을 혼동하지 않는다.

## 구현 순서와 파일 책임

| 단계 | 대상 | 완료 기준 |
|---|---|---|
| A. 계약 확정 | 인증 어댑터 명세, 상태 전환·제어 메시지 형식 | 불명확한 인증/생성/종료를 성공으로 처리하지 않는 계약 확정 |
| B. 소유권 분리 | `agy/store.rs`, `agy/rooms.rs`, 새 ownership 모듈 | read-only·100ms 초기 판정, durable ID 게시, ever-configured 표식, 충돌 우선순위 |
| C. 실행 관리자 | `agy.rs`, `agy/worker.rs`, 새 lifecycle/configuration/DB 작업자 모듈 | 공유 경계 오류 격리, 접수 commit 직렬화, 정리 확인 전 On 금지, drain·즉시 중단 |
| D. 공통 경로 연결 | runtime message/interaction/history, cdr-discord gateway publication/identity/큐, 위 mirror 정리 5개 파일 | Codex 슬롯 진입 전 분기, topology loss/epoch, DELETE·fence·retirement 전체 보호, offline exact cleanup guard |
| E. 로컬 제어 | 새 control 모듈, Windows pipe·설정 writer lock | OS 상대 신원 검증, 동일 ID/다른 payload 거절, 만료·UUID 변경·공유 설정 경로 충돌 처리 |
| F. 트레이 | 기존 tray 진입점 + AGY 전용 PowerShell helper | 비동기 메뉴, 실제 상태 표시, 공식 로그인 연결, 기존 재시작 기능 보존 |
| G. 검사·문서 | AGY/트레이/미러 검사, 기존 `docs/agy-discord.md` | 아래 계약 검사와 빌드, 실행본 미적용 표시 |

기존 기능을 길이만 맞추려고 분리하지 않는다. 새 파일은 소유권·실행 관리·제어 통신·UI라는 책임 차이가 있는 경우에만 추가한다.

## 검증 계약

가짜 Discord, 임시 DB, 가짜 CLI 프로세스, 로컬 pipe 클라이언트로 검증한다. 실제 로그인·모델·Discord 쓰기는 하지 않는다.

1. CLI 없음/손상 설정/AGY DB 잠김/인증 실패/worker panic 각각에서 Codex 시작·정상 대화가 유지된다.
2. Off/AuthRequired/Faulted 상태의 AGY 메시지·슬래시·버튼·자동완성·gap/history가 Codex 실행기에 도달하지 않는다.
3. 명시적 AGY 소유 대상, 수동 이동 대상, 소유권 충돌, 소유권 읽기 실패에 대한 mirror sync 삭제 방지가 동작한다.
4. 두 번 켜기, 켜는 도중 끄기, 늦은 인증 결과, 설정 저장 충돌, 트레이 재실행은 중복 worker·로그인·카테고리를 만들지 않는다.
5. 정상 끄기는 접수된 작업을 완료하고, 즉시 중단은 해당 AGY 프로세스 트리만 종료한다. Codex와 다른 AGY 도구 인스턴스는 보존한다.
6. 끄기 의도 저장 실패, 작업 결과 저장 실패, Discord 생성 응답 손실 각각이 명확한 오류 상태로 남는다.
7. 비활성 기간의 메시지가 활성화 후 gateway gap/history 복구로 실행되지 않는다. 경계 직전·직후와 재시작 사례를 검사한다.
8. 네트워크·사용량·모델 접근 오류를 인증 만료로 잘못 표시하지 않는다. probe 시간 초과·잘못된 JSON·버전 불일치는 Ready가 아니다.
9. 다른 Windows 사용자/다른 root/이전 runtime UUID/재사용 PID/중복 request ID/과대 요청/끊긴 pipe를 처리한다.
10. 트레이 상태 갱신 실패가 창을 멈추거나 봇 재시작을 일으키지 않는다. 이전 Ready 상태가 현재 상태처럼 남지 않는다.
11. 기존 AGY 대화 재개·stop·중복 방지·결과 전달과 기존 Codex 미러·트레이 재시작 계약을 유지한다.
12. 성능 관문의 추가 호출 수·분리된 대기열·AGY 지연 중 Codex 완료를 실제 계측한다.
13. AGY 소유권 lookup/gap 오류·handler panic·미분류 lookup 지연을 실제 공유 history/interaction 경로에 주입한다. common fatal notification 0회와 Codex 처리 지속을 검사한다.
14. AGY A 실행 중 Off→On, claim 직후/enqueue 직전 Off, 결과 저장 실패+B 실행 중 panic, 늦은 생성 응답을 검사한다. 이전 generation 정리 전 두 번째 worker/Store recovery 시작은 0회여야 한다.
15. 새 ID의 durable 기록·snapshot 게시·Ready 사이에 입력과 sync를 끼워 넣는다. 취소 뒤 생성 응답, 수동 이동, 같은 identity 재연결, topology 큐 overflow에서 stale Codex 우선 판정이 없어야 한다.
16. 위 mirror 경로 각각에 Agy/Conflict/Uncertain를 넣어 DELETE 0회, mapping retirement 0회, 잘못된 cleanup complete 0회, ID 보존을 검사한다. 확실한 Codex cleanup과 기존 pending/uncertain fence 검사는 계속 통과해야 한다.
17. Checking 입력의 지연 전달, cutoff 전후 message/interaction, 오래된 메시지의 새로운 버튼 클릭, 재시작을 구분한다. 과거 입력의 실행은 0회이며 Ready 이후의 새 요청은 정당한 정책에 따라 처리한다.
18. 가짜 동일 handshake의 다른 PID, 같은 ID/다른 payload, 결과 cache 만료·포화, 다른 runtime UUID, 두 root의 동일 canonical 설정 경로, 상태 응답 역순을 검사한다.

실행할 검사: AGY 집중 Rust 검사, mirror sync 계약, Windows 트레이 계약, 변경 영향 범위의 기존 runtime/Discord 검사, workspace 포맷, Clippy, 개발용 실행 파일 빌드. 실패가 없는 검사를 근거 없이 반복하지 않는다.

## 완료 판정과 이후 단계

- 설계 검수 통과, 로컬 구현 검증 통과, 실제 배포/통합 검증 통과를 별개의 상태로 기록한다.
- 이 문서의 Pro 통과는 향후 구현 계획에 대한 판단이다. CLI 인증 계약의 실제 증명이나 5060 통합 성공을 뜻하지 않는다.
- 이후 구현에서도 실행 중인 봇을 교체하지 않는다. 개발 실행 파일의 버전/해시와 실행 중인 파일을 구분해 보고한다.
- 5060 통합 시에는 기존 단독 AGY 봇의 중복 응답 방지와 기존 세션 이관을 먼저 결정한다. 그 작업 없이 같은 방에 두 봇을 연결하지 않는다.
- 롤백은 새 기능을 Off로 돌리고 기존 실행본을 유지하는 방법이 기본이다. 소유권·대화 기록을 삭제하거나 예전 실행본이 AGY 방을 Codex로 처리하게 만드는 롤백은 허용하지 않는다.

검수 기록은 별도 문서를 참조한다. R2는 R1의 세 차단 사항과 인증·pipe·성능 관문 지적을 반영한 계획이며, 이 문서 자체로 Pro 통과를 선언하지 않는다.
