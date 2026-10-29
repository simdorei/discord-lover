# AGY 선택 기능 설계 검수 기록

- 검수 대상: `agy-optional-lifecycle-plan.md`
- Pro 대화: https://chatgpt.com/c/6ae1f58e-67e4-83ee-8618-cb5f3f842131
- 범위: 설계·구현 계획 검수. AGY 기능 구현·배포·5060 실제 통합 검사는 보류.

## 검수 연결 복구

화면의 `추론 수준\nPro`를 검증 helper가 인식하지 못했다. 사용자가 표기 인식 수정과 추가 재시도를 승인했다. 관찰된 두 줄 표기만 허용하고 Pro·정확한 OAuth 연결·일반 Chat·사용자 초안 보호를 유지했다. 회귀 검사에서 수정 전 실패를 재현했고, 수정 후 관련 Python 검사 72개가 통과했다. 수정된 helper의 verified 결과를 받은 뒤 요청을 전송했다.

- helper 정상화 SHA-256: `68987bee6be7cdc89a3a2bc617324dca05e88f686b54900eeedb1cee673b0d4f`
- 복구 변경은 설치된 helper와 그 무결성 hash pin 및 검사에 한정했다. 브릿지 실행 파일은 빌드·교체·재시작하지 않았다.
- 기존 변경을 보존한 수정 전 사본과 이번 변경만의 patch: `D:/codex-work/cdr-agy-target-20261028/review/`

## R1: REVISE

- 검토 SHA-256: `5f6edb5430a1f6ac19f254ecd63390627710dd4d00a5bce90b078c3337e13a4a`
- Pro는 문서 전체, 지정 device/root와 관련 소스를 읽었다. 검수 중 변경은 말미의 진행 기록뿐이었고 설계 본문은 동일함을 확인했다. 이후 판부터 본문 파일과 진행 기록을 분리한다.
- 전체 답변 보존: `D:/codex-work/cdr-agy-target-20261028/review/pro-r1-complete.txt`

| Pro 지적 | 독립 확인·판단 | R2 반영 |
|---|---|---|
| 공유 ingress/history의 지연·panic·오류 격리 누락 | 수용. message 소비자의 직렬 await, interaction 공통 슬롯, history의 AGY `await?`를 직접 확인 | 공통 슬롯 전 분기, bounded 큐/DB 작업자, AGY 경계별 실패 처리와 시간 한도 |
| Draining/Faulted에서 재활성화 조건 불명확 | 수용. 결과 저장 실패가 active 제거보다 먼저 반환되고 Store open이 중단 요청을 일괄 복구함 | 정리 중 On 금지, 프로세스/task/permit 정리 증거, durable 접수 확정 시점과 Off 직렬화 |
| 소유권 topology 전달·생성 게시·정리 경계 누락 | 수용. gateway가 두 이벤트 외 Ignored, archived direct dispatch와 mapping-only retirement 존재를 확인 | gateway topology/epoch/손실 전달, ID 게시 순서, 모든 DELETE/fence/retirement 및 offline exact guard |
| 인증 probe 의미의 증명 필요 | 수용. 실증 전 차단을 유지하는 안전한 관문이라는 판단에 동의 | 인증을 필요로 하는 성공 증거·실 CLI와 fixture 구분, 미증명 probe 실행 금지 |
| pipe 상대 신원·중복·writer 충돌 계약 | 수용. root singleton만으로 공유 설정 경로의 writer 하나를 보장할 수 없음 | OS peer 검증, ID/payload/만료 규칙, canonical 설정 writer lock |
| 유한한 성능 합격 기준 필요 | 수용. AGY를 풀기 전에 완료한다는 조건만으로 느려짐을 판별할 수 없음 | 입력·반복·측정점, p95 회귀 한도, 절대 deadline, 2500ms ACK 계약 고정 |

## R2: PASS

세 차단 계약과 세 검증 관문을 반영한 고정 문서를 동일 Pro 대화에서 재검수했다. 응답 생성 종료와 완성된 답변을 확인했고, Pro는 `PASS — R2 설계·구현 계획`으로 판정했다. 추가 필수 계약 수정은 요구하지 않았다.

- 고정 SHA-256: `3a5d73040aa20e7cd8bba904251902a1a171a2c1c5ca8dfac2bc86e27f478c96`
- 고정 사본: `D:/codex-work/cdr-agy-target-20261028/review/agy-plan-r2.md`
- 요청 사본: `D:/codex-work/cdr-agy-target-20261028/review/r2-pro-request.txt`
- 전체 완료 답변: `D:/codex-work/cdr-agy-target-20261028/review/pro-r2-complete.txt`
- 검수 대상 본문은 이 해시 그대로 보존한다.

### 독립 평가와 채택

- B1: 실제 공유 message/interaction/history 진입점과 대조한 분기·대기·오류 격리 계약이므로 해소 판정을 수용한다. 전용 실행 worker만 추가해서 완료로 처리하지 않는다.
- B2: 접수 확정과 Off 직렬화, 이전 generation 정리 완료 조건이 문서에 존재하므로 해소 판정을 수용한다. 250ms 경과를 rollback 확정으로 해석하면 안 된다는 Pro의 설명은 기존 L82·L90·L99 계약의 적용이며, 구현 검사에서 증명할 사항이다.
- B3: 직접 DELETE와 원격 부재 처리, 로컬 mapping retirement를 포함한 다섯 정리 경로가 보호 표에 포함되므로 해소 판정을 수용한다. 기존 pending-work·uncertain fence를 보존하는 검사도 유지한다.
- 인증 probe와 구체 cutoff 알고리즘은 증명 전 활성화를 차단하는 후속 관문으로 남는다. 계획 PASS를 이들의 실제 검증 성공으로 취급하지 않는다.
- pipe 신원·설정 경로 잠금·중복 제어와 성능 수치는 앞으로 로컬 구현 검사에서 검증할 계약이다. 실제로 검사하거나 측정했다고 주장하지 않는다.

### 완료 범위

| 대상 | 상태 |
|---|---|
| Pro 표기 인식 오류 수정 | 수정 전 실패 재현, 수정 후 관련 검사 72개 통과, 실제 연결 verified |
| AGY 선택 기능 설계·구현 계획 | R1 REVISE → R2 PASS |
| AGY 기능 구현·빌드·실행본 교체 | 이번 검수에서 수행하지 않음 |
| 실제 CLI 인증 확인의 안전성·인증 의미 | 미검증, 후속 관문 |
| 5060 적용·실제 Discord 통합·성능 실측 | 사용자 지시에 따라 계속 보류 |

현재 개발 worktree의 기존 변경과 운영 중인 브릿지를 보존했다. 이 기록은 계획 검수 결과이며 배포 승인이나 실제 동작 보증이 아니다.
