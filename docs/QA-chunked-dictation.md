# Manual QA: chunked dictation

Run on macOS and Windows before a release that touches `segmenter.rs`,
`segment_jobs.rs`, or the stop flow. Automated coverage lives in the
`segmenter`, `segment_jobs`, `engine`, `dictation`, and `settings` test modules.

Use the default model (Parakeet v3) unless a row says otherwise. Watch the log
line `segmented take (...)` after each dictation; it shows the pause source
(`silero` or `energy`), the segment count, forced cuts, and per-segment timing.

## Setup

- [ ] Settings -> "Transcribe while you talk" is on (default for upgraded installs).
- [ ] Log level shows `dictation segmenting on (silero, ...)` about 2 s into a take.

## Core behavior

- [ ] Short take (under 10 s): normal result, no `segmented take` log line.
- [ ] 2-minute take with a short pause every 10 to 15 seconds: text appears
      within a few seconds of stopping; every sentence present, in order.
- [ ] 2-minute take talking continuously with no real pauses: forced cuts are
      logged, no word is doubled or cut in half at a seam.
- [ ] Say "save fifty" then pause 1 s then "percent off": result has `50%`.
- [ ] Say "draft an email to Sam" pause "telling him I will be late" pause
      "cheers, Alex": result is a composed email.
- [ ] A list dictated across a pause keeps its numbering.
- [ ] A 1 s mic test in Setup does not log a segmenter line.

## Edge cases

- [ ] Stop right after a pause (tail is silence): no extra empty text, no error.
- [ ] Stop in the middle of a word: last word is present.
- [ ] Stay silent for 30 s, then say one sentence: only that sentence appears.
- [ ] Only silence for a long take: "only silence" style blank-audio message.
- [ ] Cancel (talk key plus modifier) during a long take: nothing pasted, no
      history entry, no leftover file in `recordings/`.
- [ ] Start a new take right after stopping a long one: no overlap, no stale text.
- [ ] Quiet speaker / noisy room: cuts still land in pauses (compare the log).
- [ ] Switch the microphone mid-session between takes: next take segments normally.
- [ ] Settings -> turn "Transcribe while you talk" off: a long take is decoded
      whole and no `segmented take` line is logged.

## Engines

- [ ] Parakeet: segments about 10 to 30 s.
- [ ] Whisper sidecar: segments about 20 to 40 s; no `seg-*` files left in the
      data folder after the take.
- [ ] Cloud audio API: segments about 20 to 40 s; result matches a whole-take run.
- [ ] Model cleanup / S1-mini on: runs once on the joined text (one cleanup
      request per take in the log).

## Fallbacks

- [ ] Rename `resources/silero_vad.onnx` away: log shows `energy`, dictation works.
- [ ] Corrupt the VAD file (truncate it): app does not crash, falls back to `energy`.
- [ ] Break the engine on purpose for one segment (cloud API with a bad key):
      the take reports the error instead of pasting partial text.

## Import

- [ ] Transcribe a 2-minute WAV from the Dictate view: result matches a
      segmented live dictation of the same audio.

## Real-model smoke test

```
DICTFLOW_TEST_WAV=/path/to/long.wav \
DICTFLOW_TEST_DATA="$HOME/Library/Application Support/com.dictflow.app/dictflow" \
  cargo test --locked real_model_segmented -- --ignored --nocapture
```

Expected: word difference under 5 percent (fillers ignored), and the segmented
transcript is not shorter than the whole-file one.
