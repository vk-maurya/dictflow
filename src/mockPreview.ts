/** Sample data for LinkedIn screenshots. Active only with ?mock=1. */

export function mockEnabled(): boolean {
  return new URLSearchParams(location.search).has("mock");
}

export function mockView(): string | null {
  return new URLSearchParams(location.search).get("view");
}

function unixHoursAgo(hours: number): number {
  return Math.floor(Date.now() / 1000) - hours * 3600;
}

function ymd(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function dayBucket(words: number, dictations: number, mins: number) {
  return {
    dictations,
    words,
    audio_secs: mins * 60,
    raw_words: Math.round(words * 1.08),
    dict_hits: Math.max(1, Math.round(dictations * 0.4)),
  };
}

export function mockState() {
  const by_day: Record<string, ReturnType<typeof dayBucket>> = {};
  const pattern = [820, 640, 910, 480, 1100, 260, 740, 980, 520, 860, 410, 1200, 690, 330, 770];
  for (let i = 0; i < 21; i++) {
    const d = new Date();
    d.setHours(12, 0, 0, 0);
    d.setDate(d.getDate() - i);
    const words = pattern[i % pattern.length];
    by_day[ymd(d)] = dayBucket(words, 4 + (i % 5), 6 + (i % 8));
  }

  const history = [
    {
      text: "Stop typing. Hold a key, speak, and polished text lands in the app you already have open.",
      date_unix: unixHoursAgo(1),
      duration_secs: 8.6,
      model: "parakeet-v3",
      audio_path: "rec-demo-1.wav",
      words_out: 18,
      dict_hits: 0,
      cleanup: "full",
    },
    {
      text: "Emails, docs, Slack, and code comments. Dictate anywhere. Nothing is uploaded.",
      date_unix: unixHoursAgo(3),
      duration_secs: 7.4,
      model: "parakeet-v3",
      audio_path: "rec-demo-2.wav",
      words_out: 12,
      dict_hits: 1,
      cleanup: "full",
    },
    {
      text: "Your voice stays on your computer. One model download, then DictFlow works with the wifi off.",
      date_unix: unixHoursAgo(26),
      duration_secs: 9.1,
      model: "parakeet-v3",
      audio_path: null,
      words_out: 16,
      dict_hits: 1,
      cleanup: "full",
    },
    {
      text: "Write three times faster than typing, with clean punctuation and your own dictionary.",
      date_unix: unixHoursAgo(28),
      duration_secs: 7.8,
      model: "parakeet-v2",
      audio_path: "rec-demo-4.wav",
      words_out: 13,
      dict_hits: 0,
      cleanup: "light",
    },
    {
      text: "Open source for Windows and Mac. Download it, talk, and paste. That is the whole product.",
      date_unix: unixHoursAgo(50),
      duration_secs: 8.2,
      model: "parakeet-v3",
      audio_path: null,
      words_out: 16,
      dict_hits: 0,
      cleanup: "full",
    },
  ];

  const lifetime_words = Object.values(by_day).reduce((n, b) => n + b.words, 0);

  return {
    view: mockView(),
    lastResult:
      "DictFlow turns your voice into text, right where you are typing. Private, offline, and ready in any app.",
    status: {
      recording: false,
      model_id: "parakeet-v3",
      engine: "parakeet" as const,
      engine_label: "Parakeet",
      model_loaded: true,
      whisper_binary: null,
      data_dir: "~/Library/Application Support/com.dictflow.app/dictflow",
      audio_device: "MacBook Pro Microphone",
      version: "0.3.0",
      recommended_model: "parakeet-v3",
      transcribing: false,
      speech_model_name: "Parakeet TDT 0.6B v3",
    },
    models: [
      {
        id: "parakeet-v3",
        name: "Parakeet TDT 0.6B v3",
        engine: "parakeet" as const,
        languages: "25 languages",
        size_label: "~670 MB",
        speed: 9.7,
        accuracy: 9.2,
        essence: "Best all-rounder",
        min_ram_gb: 4,
        files: [],
        downloaded: true,
        active: true,
        downloading: false,
      },
      {
        id: "parakeet-v2",
        name: "Parakeet TDT 0.6B v2",
        engine: "parakeet" as const,
        languages: "English-only",
        size_label: "~660 MB",
        speed: 9.8,
        accuracy: 9.1,
        essence: "Fastest English",
        min_ram_gb: 4,
        files: [],
        downloaded: true,
        active: false,
        downloading: false,
      },
      {
        id: "small.en",
        name: "Whisper Small",
        engine: "whisper" as const,
        languages: "English-only",
        size_label: "~470 MB",
        speed: 8.0,
        accuracy: 8.5,
        essence: "Reliable",
        min_ram_gb: 4,
        files: [],
        downloaded: false,
        active: false,
        downloading: false,
      },
      {
        id: "base.en",
        name: "Whisper Base",
        engine: "whisper" as const,
        languages: "English-only",
        size_label: "~145 MB",
        speed: 9.0,
        accuracy: 7.5,
        essence: "Fast & light",
        min_ram_gb: 2,
        files: [],
        downloaded: false,
        active: false,
        downloading: false,
      },
    ],
    history,
    usage: {
      lifetime_dictations: 186,
      lifetime_words,
      lifetime_audio_secs: Object.values(by_day).reduce((n, b) => n + b.audio_secs, 0),
      lifetime_raw_words: Math.round(lifetime_words * 1.08),
      lifetime_dict_hits: 74,
      by_day,
    },
    dict: [
      { id: "1", trigger: "my email", replacement: "hello@dictflow.app", enabled: true, whole_word: true, kind: "vocab", starred: true, usage: 18 },
      { id: "2", trigger: "product name", replacement: "DictFlow", enabled: true, whole_word: true, kind: "vocab", starred: true, usage: 42 },
      { id: "3", trigger: "tagline", replacement: "offline voice dictation", enabled: true, whole_word: true, kind: "vocab", starred: false, usage: 9 },
      { id: "4", trigger: "sign off", replacement: "Best regards, the DictFlow team", enabled: true, whole_word: true, kind: "snippet", starred: true, usage: 6 },
      { id: "5", trigger: "pitch", replacement: "Hold a key. Speak. Text lands in any app.", enabled: true, whole_word: true, kind: "snippet", starred: false, usage: 4 },
    ],
    settings: {
      model_id: "parakeet-v3",
      language: "auto",
      auto_paste: true,
      cleanup: "full",
      translate: false,
      hotkey_key: "Fn",
      recording_mode: "hold",
      audio_device: "MacBook Pro Microphone",
      overlay_enabled: true,
      overlay_edge: "bottom",
      overlay_offset: 24,
      overlay_free: false,
      overlay_x: 0,
      overlay_y: 0,
      paste_last_key: "Shift+Alt+Z",
      copy_last_key: "Shift+Alt+X",
      refine_selection_key: "Alt+1",
      onboarded: true,
      session_cap: false,
      audio_backend: "local",
      audio_api_base: "",
      audio_api_model: "",
      llm_backend: "off",
      llm_api_base: "",
      llm_api_model: "",
      llm_temperature: 0.2,
      llm_timeout_ms: 8000,
      llm_preset: "clean",
      llm_custom_prompt: "",
      llm_enabled: false,
    },
    provider: {
      audio_key_masked: "",
      llm_key_masked: "",
      has_audio_key: false,
      has_llm_key: false,
      default_prompt: "",
      prompt_presets: {},
    },
    audioDevices: [
      { name: "MacBook Pro Microphone", sample_rate: 48000, channels: 1, is_default: true, is_selected: true },
      { name: "External USB Mic", sample_rate: 44100, channels: 1, is_default: false, is_selected: false },
    ],
    perms: { microphone: true, accessibility: true, input_monitoring: true, talk_key_live: true },
  };
}
