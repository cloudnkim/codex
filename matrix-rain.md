# Matrix 효과와 공식 릴리즈 자동 반영

## 소스 기준

- `cloudnkim/codex`는 `openai/codex`의 공개 포크다.
- `matrix` 브랜치는 공식 정식 릴리즈 태그 `rust-vX.Y.Z`를 따라간다.
- 현재 공식 버전은 [matrix-upstream-version.txt](matrix-upstream-version.txt)에 기록한다.
- 최초 기준 버전은 [matrix-baseline-version.txt](matrix-baseline-version.txt)에 보존한다.
- 원본 `main`의 미출시 변경은 자동으로 반영하지 않는다.
- `matrix` 브랜치에는 전용 릴리즈 워크플로만 둔다.
- 공식 소스 병합 시 `.github/workflows`는 기존 `matrix` 설정을 유지한다.
- [기존 패치 저장소와 첫 릴리즈](https://github.com/cloudnkim/codex-matrix-rain/releases/tag/v0.160.1)는 보존한다.
- 공식 테스트 파일과 이력은 포크에 유지한다.
- 기존 효과의 별도 테스트 파일은 추가하지 않는다.

## 실행

- `CODEX_MATRIX_RAIN=1`과 TUI 애니메이션 설정이 켜져 있으면 출력 영역의 빈 셀에 효과를 표시한다.
- 입력창·출력 문자·대시보드에는 효과를 표시하지 않는다.
- TrueColor 또는 ANSI 256색 터미널을 사용한다.
- 릴리즈 패키지의 `bin/codex-matrix`는 효과를 켜고 CLI 인자를 그대로 전달한다.
- 패키지의 `bin/codex`는 실행 환경의 효과 설정을 따른다.
- 다른 Apple Silicon 맥에서는 릴리즈 패키지의 압축을 풀고 `sh ./install.command`로 설치한다.
- 설치한 `~/.codex/bin/codex-matrix-X.Y.Z`는 해당 버전을 실행한다.
- 설치에는 Rust·Node.js·Python이 필요하지 않다.
- 기존 설정·인증·명령은 보존하고 버전별로 별도 설치한다.
- 버전별 설치 명령은 로컬 자동 업데이트 런처와 별도로 사용한다.

```sh
cd codex-rs
cargo build --locked --release -p codex-cli --bin codex
CODEX_MATRIX_RAIN=1 ./target/release/codex
```

## 상태 표시

- Matrix 모드에서는 상태줄 모델명 왼쪽에 고정 11칸 스캔 표시를 붙인다.
- 계획 단계의 완료 비율에 따라 `CODEX ÉXITO`의 글자를 공개한다.
- 계획 진행도를 모르면 스캔과 미확정 문자만 표시한다.
- 정상 완료일 때 마지막 글자를 공개하고 3초 뒤 `CODEX READY`로 돌아간다.
- 중단·실패·기록 재생에는 성공 문구를 표시하지 않는다.
- 애니메이션을 끄면 문자는 정적으로 표시하고 완료 문구의 만료만 갱신한다.
- 좁은 화면이나 다른 안내가 상태줄을 가리면 스캔 갱신을 중단한다.

## 로컬 자동 업데이트

- `codex-sol`과 `codex-astra`는 공통 Matrix 런처에서 실행 전 최신 포크 릴리즈를 확인한다.
- 새 패키지의 체크섬·서명·버전을 확인한 뒤 설치한다.
- 설치가 완료된 패키지로 실행 경로를 한 번에 전환한다.
- 이전 패키지는 보존한다.
- 이미 실행 중인 Codex는 해당 패키지를 계속 사용한다.
- 새 빌드가 게시된 뒤 다음 실행부터 새 버전을 사용한다.
- 다운로드·검증 실패나 다른 업데이트 실행 중에는 기존 버전으로 실행한다.
- `sol`·`astra` 프로필과 CLI 인자는 그대로 전달한다.
- `~/.codex/packages/matrix/fallback`을 기존 Matrix 패키지 디렉터리에 연결한다.
- Python `3.12` 이상을 사용한다.

```sh
mkdir -p "$HOME/.codex/bin"
install -m 755 scripts/update-matrix.py "$HOME/.codex/bin/update-matrix.py"
install -m 755 scripts/matrix-launcher.sh "$HOME/.codex/bin/codex-matrix"
```

- 실행 도구: [scripts/update-matrix.py](scripts/update-matrix.py)
- 공통 런처: [scripts/matrix-launcher.sh](scripts/matrix-launcher.sh)

## 예약

- 기본 브랜치 `matrix`에서 `Codex Matrix 공식 릴리즈 반영` 워크플로를 실행한다.
- 매일 `08:00 KST`를 `0 23 * * *` UTC cron으로 예약한다.
- GitHub 대기열에 따라 시작 시각이 늦어질 수 있다.
- 같은 작업은 동시에 실행하지 않는다.
- 새 공식 버전이 없으면 빌드와 게시를 생략한다.
- 최초 `0.160.1`은 기존 저장소의 소스 릴리즈를 완료된 기준으로 취급한다.
- 현재 버전의 미완료 게시가 있으면 더 새 버전보다 먼저 재시도한다.

## 업데이트와 게시

1. 최신 공식 정식 릴리즈를 확인한다.
2. 공식 태그를 `matrix` 소스에 병합하고 버전 파일을 갱신한다.
3. 해당 소스의 고정 Rust 툴체인으로 `codex-cli`의 `codex` 릴리스 바이너리만 빌드한다.
4. CLI 버전이 공식 버전과 일치하는지 확인한다.
5. 공식 macOS Apple Silicon 전체 패키지의 SHA-256을 검증한다.
6. 패키지의 CLI를 교체하고 보조 실행 파일·리소스·라이선스·설치 스크립트를 포함한다.
7. 실행 래퍼·서명·구성·SHA-256을 확인한다.
8. 빌드한 소스 커밋을 `matrix`에 일반 푸시한다.
9. 초안 릴리즈에 패키지와 체크섬을 첨부한다.
10. 첨부 파일을 검증한 뒤 정식 릴리즈로 게시한다.

- 빌드 대상은 `aarch64-apple-darwin`이다.
- 확인·빌드 작업에는 저장소 읽기 권한만 부여한다.
- 게시 작업에만 저장소 쓰기 권한을 부여한다.
- 소스 푸시는 이 포크 전용 배포 키 `MATRIX_PUSH_KEY`를 게시 작업에서만 사용한다.
- 릴리즈 API는 게시 작업의 `GITHUB_TOKEN`을 사용한다.
- 자동 검증은 병합·컴파일·버전·패키지 무결성을 확인한다.
- 효과의 실제 화면·입력·스크롤 동작은 터미널에서 별도로 확인한다.

## 실패 처리

- 병합 충돌·컴파일·패키지 검증 실패 시 원격 소스와 공개 릴리즈를 갱신하지 않는다.
- 병합 충돌은 효과 코드를 수동으로 수정한 뒤 다시 실행한다.
- 소스 반영 후 게시가 실패하면 다음 실행에서 해당 버전 게시를 먼저 복구한다.
- 이미 공개된 릴리즈와 태그는 덮어쓰거나 삭제하지 않는다.
- 빌드 중 기본 브랜치가 바뀌면 작업을 중단한다.
- 강제 푸시와 자동 rebase는 사용하지 않는다.
- 실패 로그는 GitHub Actions 실행 기록에서 확인한다.

## 수동 실행

1. 저장소의 `Actions`에서 `Codex Matrix 공식 릴리즈 반영`을 선택한다.
2. 기본 브랜치 `matrix`의 `Run workflow`를 실행한다.

- 워크플로: [.github/workflows/matrix-release.yml](.github/workflows/matrix-release.yml)
- 실행 도구: [scripts/matrix-release.py](scripts/matrix-release.py)
- 예약 기준: [GitHub schedule 이벤트](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#schedule)
