# Discord Lover 이전 기록

2026-10-29에 Codex+AGY 개발본 전체를 별도 저장소
[`simdorei/discord-lover`](https://github.com/simdorei/discord-lover)로 옮겼다.
AGY 단독 저장소인 [`simdorei/agy-discord-remote`](https://github.com/simdorei/agy-discord-remote)와는
별개이며, Codex와 선택형 AGY를 함께 개발한다. Claude Code 지원은 아직 추가하지 않았다.

## 보존한 소스와 이력

- 기반: `simdorei/codex-discord-remote-rust`, commit `2aaddf87ee037d3ae6dfb5d9d87489430d6ce74e`.
- 개발본 원본: `feat/agy-discord-category-20261028` 작업 폴더.
- 전체 개발본 보존 commit: `2649a13e8b7b0d838bc1b2449f669220ddee9dd4`.
- 기반 Git 이력 85개와 개발 파일 2,118개를 보존했다. 기존 파일 변경 39개와 새 파일 50개가 개발본 commit에 포함된다.
- [고정 R3 manifest](agy-implementation-review-r3.json)의 84개 파일은 위 보존 commit과 모두 일치한다.
- 이후 이전 안내와 README의 이름·설치 주소만 갱신했다. R3 manifest는 현재 README가 아니라 위 보존 commit을 가리킨다.
- 실행 프로그램 이름, 설정 키, 플러그인 식별자 및 동작은 기존 개발본을 유지한다.

Git 대상 소스와 개발 문서를 옮겼으며 로컬 인증 정보, 실제 대화 기록, 데이터베이스,
빌드 결과 및 실행 중인 프로그램은 이전 대상에 포함하지 않았다.
기존 Codex/AGY 저장소와 운영 중인 브릿지를 변경하거나 재시작하지 않았다.

## 검증 범위와 남은 작업

이전한 개발본은 [구현 및 Pro 검수 기록](agy-implementation-progress.md)의 R3 PASS 대상이다.
기존 검증 결과는 runtime 630개와 관련 회귀 142개 통과, 개발 빌드 및 로컬 성능 검사 통과다.
이번 이전에서는 파일 내용과 Git 이력 보존을 대조했으며, 이 결과를 실제 서비스 통합 성공으로 해석하지 않는다.

AGY는 기본 Off다. 실제 인증 확인과 메시지 접수 시작 경계의 운영 증거가 아직 연결되지 않아
production 어댑터가 `VerificationRequired`에서 멈춘다. 이 상태에서는 실제 AGY 작업과 카테고리 생성을 시작하지 않는다.
실제 로그인, 모델 호출, Discord 변경, 5060 PC 통합 및 운영 배포는 아직 수행하지 않았다.
[AGY 개발본 안내](agy-discord.md)에 현재 기능과 보류 범위를 정리했다.

이전 검증에서 변경 Rust 파일의 형식 검사는 통과했지만, 기존 미수정
`crates/cdr-runtime/src/action_executor/new_thread/verification_tests.rs`의 형식 문제는 남아 있다.
전체 workspace 형식 검사가 통과했다고 주장하지 않으며 이번 저장소 이전에서 해당 코드를 변경하지 않았다.
