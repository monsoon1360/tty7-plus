package io.tty7.mobile

internal data class AgentAppearance(val kind: String, val field: Int, val ink: Int = 0xFFFFFF)

// Match the daemon's display names and the desktop's cli_agent.rs palette.
internal fun agentAppearance(name: String?): AgentAppearance? = when (name?.trim()?.lowercase()) {
    null, "" -> null
    "claude code", "claude" -> AgentAppearance("claude", 0xD97757)
    "codex" -> AgentAppearance("codex", 0x000000)
    "traecode", "traecli" -> AgentAppearance("traecli", 0x000000, 0x32F08C)
    "gemini" -> AgentAppearance("gemini", 0x4285F4)
    "aider" -> AgentAppearance("aider", 0x14B014)
    "amp" -> AgentAppearance("amp", 0xF34E3F)
    "opencode" -> AgentAppearance("opencode", 0x6E56CF)
    "copilot" -> AgentAppearance("copilot", 0x8957E5)
    "cursor" -> AgentAppearance("cursor", 0x9AA0A6)
    "goose" -> AgentAppearance("goose", 0x3ECC5F)
    "droid" -> AgentAppearance("droid", 0xEF6F2E)
    "pi" -> AgentAppearance("pi", 0x0EA5E9)
    "auggie" -> AgentAppearance("auggie", 0x16A34A)
    "hermes" -> AgentAppearance("hermes", 0x8B5CF6)
    "vibe" -> AgentAppearance("vibe", 0xFA520F)
    "antigravity" -> AgentAppearance("antigravity", 0x3186FF)
    "grok" -> AgentAppearance("grok", 0x000000)
    "qwen code", "qwen" -> AgentAppearance("qwen", 0x6D44E8)
    "oh my pi", "omp" -> AgentAppearance("omp", 0xF97316)
    "kimi code", "kimi" -> AgentAppearance("kimi", 0x027AFF)
    "qoder cli", "qodercli" -> AgentAppearance("qodercli", 0xFFFFFF, 0x000000)
    "qoder cn cli", "qoderclicn" -> AgentAppearance("qoderclicn", 0xFFFFFF, 0x000000)
    "crush" -> AgentAppearance("crush", 0x6B50FF)
    "codebuddy" -> AgentAppearance("codebuddy", 0x1F1F1F)
    "empryo" -> AgentAppearance("empryo", 0xE8663D)
    "muse code" -> AgentAppearance("muse", 0x0866FF)
    "jcode" -> AgentAppearance("jcode", 0x111111)
    "prime agent" -> AgentAppearance("primeagent", 0x111111)
    else -> AgentAppearance("unknown", 0x6B7280)
}
