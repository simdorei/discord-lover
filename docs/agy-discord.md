# 선택형 AGY 개발본

기존 Rust Discord 봇에 AGY 전용 **카테고리**를 붙이는 개발 기능이다. Codex와 AGY의 대화 저장소·실행 대기열·오류 처리를 분리한다. 기본값은 Off이며, 현재 실행 중인 3060/5060 브릿지에는 적용하지 않았다.

## 현재 사용 가능 범위

트레이 메뉴, 설정 저장, 상태 전환, 별도 입력 경로, 방 소유권 보호와 종료 관리 코드를 구현했다. 실제 인증을 안전하게 증명하는 수단과 Discord 접수 경계를 실증하는 관문은 남아 있다. 따라서 **현재 production 어댑터는 VerificationRequired에서 멈추며 실제 AGY 작업이나 카테고리 생성을 시작하지 않는다.** 가짜 환경의 Ready 검사는 실제 로그인·배포 성공을 뜻하지 않는다.

실제 CLI 로그인·모델 호출·Discord 쓰기·5060 통합·기존 봇 재시작은 사용자가 보류한 범위다. 토큰이나 쿠키를 읽거나 옮기지 않는다.

## 설정과 트레이

설정 위치는 `DISCORD_AGY_CONFIG` 또는 기본 `<runtime root>/codex-agy.json`이다. **설정 경로가 있다는 이유로 켜지지 않는다.** 이전 개발용 직접 설정 형식은 enabled=false, revision=0으로 읽는다. 아직 배포하지 않은 개발 기능의 의미 변경이다.

```json
{
  "version": 1,
  "revision": 1,
  "enabled": false,
  "config": {
    "executable": "C:/path/to/agy.exe",
    "projects": [{"name": "example", "cwd": "C:/work/example"}],
    "auto_approve": false
  }
}
```

트레이의 AGY 메뉴는 Enable, Disable after current work, Stop AGY work now, Open official AGY login, Check again, AGY settings로 구성된다. 설정은 런타임만 저장하며, 다른 창에서 먼저 바뀐 설정을 덮어쓰지 않는다. 같은 설정 파일을 두 런타임이 동시에 쓰지 못하게 잠근다. 기존 Discord 서버·봇·허용 사용자 설정을 재사용한다.

공식 로그인은 사용자가 메뉴를 눌렀을 때 같은 Windows 사용자·대화형 세션에서만 연다. 창 종료만으로 인증 성공을 판단하지 않는다. 자동 로그인이나 자동 인증 조회는 하지 않는다. 네트워크·사용량·모델 오류를 로그아웃으로 추측하지 않으며, 실제 오류 형식별 인증 판정은 위 미완료 관문에 속한다.

트레이는 백그라운드에서 상태를 조회한다. 응답이 오래되면 Unavailable을 표시하고 조작을 막는다. 연결이 끊긴 변경 요청은 같은 ID의 결과만 확인하며 재실행하지 않는다. 런타임이 바뀌면 이전 명령을 재전송하지 않는다. 트레이를 닫아도 봇을 재시작하거나 AGY 설정을 바꾸지 않는다.

## 인증·접수 관문 통과 후의 동작

AGY 카테고리 아래 프로젝트 채널에 질문하면 새 대화 스레드를 만들고, 스레드 안의 질문은 저장한 공식 CLI 대화 번호로 이어간다. `!new`·`!ask`·`!stop`·`!status`·`!help`와 대응하는 슬래시 명령을 처리한다. Codex 명령이나 버튼은 AGY 방에서 실행하지 않는다. 첨부파일 전달·Discord 승인 UI·전체 대화 목록 동기화는 범위 밖이다.

Off는 새 접수를 막고 이미 접수한 작업을 기다린다. 즉시 중단은 소유한 AGY 프로세스 트리만 종료한다. 종료가 확인되지 않으면 FaultedBlocked를 표시하고 다시 켜기를 막는다. Off/Checking 때의 입력은 나중에 실행하지 않으며, 접수 경계를 저장한 뒤에만 Ready가 된다.

카테고리·채널·대화 ID는 AGY 전용 SQLite와 소유권 표식에 보존한다. AGY를 꺼도 Codex sync 삭제·정리·매핑 제거가 AGY 방을 건드리지 못한다. 소유권 충돌·손상 DB·확인되지 않은 생성 결과는 성공으로 처리하지 않는다. 이름만 같은 기존 방을 임의로 인수하지 않는다. 기존 단독 AGY 봇의 세션 자동 이관은 포함하지 않는다.

## 검증과 배포 상태

계획 기준과 최신 검사 결과는 [구현 기록](agy-implementation-progress.md)에 남긴다. 로컬 검사는 임시 DB·가짜 Discord·소유한 출력용 프로세스·Windows 로컬 pipe를 사용한다. 실제 AGY 서비스에 접속하지 않는다.

개발 브랜치는 `feat/agy-discord-category-20261028`, 기준 커밋은 `2aaddf8`이다. 개발 빌드 경로는 `D:/codex-work/cdr-agy-target-20261028`이며, 운영 `target/release/cdr-runtime.exe`를 덮어쓰지 않는다. 실제 통합 전에는 인증/접수 관문, 기존 AGY 봇의 중복 응답 방지, 세션 이관 여부를 확인해야 한다.
