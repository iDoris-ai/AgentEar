# V1 Development Acceptance Results

Initial run measured 2026-09-05 on macOS 26.6.2 (25G83), arm64, Python 3.14.6.
Branch: `arm/agentear-dev`. No Rust integration or GitHub submission performed.

## Reproduction

From the repository root, in a normal macOS Terminal:

```bash
python3 -m unittest discover -s services/tts -p 'test_*.py' -v
python3 services/tts/check_macos.py
```

The first command passed 16 tests. HTTP tests substitute deterministic audio
fixtures; lifecycle tests also start real Python child processes.
The second command uses the real macOS tools and curl, not those fixtures.
It prints its artifact directory and writes full measurements, exact input
texts, frame counts, and PCM SHA-256 values to `results.json` there.

## Measured Checks

These are individual development measurements, not averages or performance
guarantees. Request times below are curl's `time_total` in seconds, including
the response transfer, not the playback duration.

| Task check | Observed result |
| --- | --- |
| Three languages | All returned `200 audio/wav`, mono 22050 Hz, 16-bit PCM, with complete non-empty frame data. Thai: 1.132296 s; English: 0.879415 s; Chinese: 0.879688 s. |
| Unsupported `jp` | `400 application/json`: `lang must be exactly one of: zh, en, th.`; 0.000763 s. |
| Empty text | `400 application/json`: `text must be a non-empty string.`; 0.000613 s. Health still returned 200 afterward. |
| 500 characters | `"hello " * 83 + "hi"`, English: 200 in 1.041494 s; WAV contained 622008 frames (28.209 s of audio). |
| Five simultaneous requests | Five distinct English inputs, all 200. Each PCM SHA-256 matched its own individually synthesized baseline. Batch wall time: 2.802914 s; individual HTTP times: 2.772527-2.788129 s. |
| Ctrl+C / SIGINT | Sent SIGINT to the service while an owned `say` process was observed. Service exited with code 0 in 0.018904 s; observed `say` PID was absent from the process table afterward; no service stderr. |
| Process cold start | 0.103202 s from launching a new Python service to receiving a successful `/health` response, including the first curl invocation. |
| English sentence, 20 words | 0.880727 s; generated audio duration 5.782 s. |
| Chinese sentence, 20 characters | 0.883579 s; generated audio duration 4.660 s. |

## Listening Evidence

Arm confirmed that the earlier direct-`say` Thai, English, and Chinese samples
were correct. After the HTTP run, a separate comparison found that all three
HTTP responses had exactly the same PCM hashes as those listened-to samples:

| Language | PCM SHA-256 |
| --- | --- |
| th | `d6fd6b45bc6cbe5b281e3ff59494962cdf4f89f49ba99581bccca412af9dd26c` |
| en | `f1c7e96705505393f5bb3dda673fe5b5ef80ab62fe1a91366fddd97e22b23866` |
| zh | `927300bcf9a07191c653ad3477d0b87babb786d0d8cc9865377fdd8de7190f12` |

This links the HTTP output to previously confirmed speech; it is not a claim
that the automated script can judge pronunciation. The script itself always
marks listening as pending, because it has no human-listening input.

## Arm-Run Verification (2026-09-06)

Arm ran both reproduction commands in Terminal. The unit-test output reported
16 tests passed in 0.441 s; this is test-suite runtime, not speech latency.
The real HTTP run reported `automated_checks: passed` on macOS 26.6.2,
arm64, Python 3.14.6. Its `results.json` and WAVs are in the temporary
artifact directory `agentear-tts-http-9e5iihxj`, not in the Git repository.

| Task check | Observed result |
| --- | --- |
| Three languages | All `200 audio/wav`, mono 22050 Hz, 16-bit PCM. Thai: 1.242255 s; English: 1.035619 s; Chinese: 0.934474 s. |
| Unsupported `jp` | 400 with the language explanation; 0.000850 s. |
| Empty text | 400 with the non-empty-string explanation; 0.000645 s. |
| 500 characters | 200 in 1.192350 s; audio duration 28.209 s. Same input as the initial run. |
| Five simultaneous requests | All five returned 200 and matched their individual PCM baselines. Batch wall time: 3.053857 s; individual HTTP times: 2.669543-3.039628 s. |
| Ctrl+C / SIGINT | Service exited with code 0 in 0.072420 s. Observed owned `say` PID 44796 was absent afterward. |
| Process start to health | 0.129520 s, including the first curl invocation. Not a post-reboot voice cold start. |
| English sentence, 20 words | HTTP time 1.032364 s; generated audio duration 5.782 s. |
| Chinese sentence, 20 characters | HTTP time 0.993692 s; generated audio duration 4.660 s. |

The three language PCM hashes match the previously confirmed samples in the
listening-evidence table above. This run's script still marks listening as
pending; no separate listening session for these new files has been reported.

Keep these measurements separate from the initial run; neither run is an
average or a timing guarantee. Individual sequential concurrency baselines
in this run ranged from 1.561733 to 4.097064 s. The cause of that variation
was not investigated; do not infer a general parallel speedup from this run.

## User-Reported Offline Verification (2026-09-07)

Arm reported running `python3 services/tts/check_macos.py` with Wi-Fi turned
off and confirmed that no other internet connection remained. The reported
result was:

```json
{"automated_checks": "passed"}
```

This supports offline operation for the script's tested cases on this Mac
with the voices already installed. Network disconnection was confirmed by
Arm, not detected by the script; no packet capture or network-interface
audit was performed. It does not establish offline installation or prove
that every possible code path makes no external connection attempts.

No full report, new timings, WAV hashes, or separate listening result were
provided for this run. The September 5 and 6 measurements above remain
separate and must not be presented as this offline run's measurements.

## Limits And Open Clarification

- The process cold-start measurement does not represent a reboot-cold macOS
  speech engine. Voices had already been exercised in this session.
- The task's Chinese section says about 20 characters, while English/Thai
  say about 20 words. Both the Chinese 20-character and English 20-word
  cases were measured and labeled separately; confirm the intended
  benchmark convention with the owner before presenting it as agreed.
- Tests on this machine needed execution outside the sandbox: loopback
  binding was denied inside it, and earlier sandboxed synthesis returned
  zero-frame audio despite successful command exit codes.
- The acceptance script does not measure RAM. Separate scratch memory
  observations were collected on September 6 and are not included here.
- No post-reboot timing, other macOS version, long-running load test, or
  Rust-side integration was performed.
