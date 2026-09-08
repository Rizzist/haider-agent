package ai.diffforge.haider.ui.components

import androidx.compose.ui.graphics.Color

/**
 * Brand catalogue for the model a session is on, ported from the desktop
 * client's `modelBrandCatalog.js`.
 *
 * Detection is substring-based over the **model id first** and the provider id
 * only as a fallback, so a new model name — `gpt-5.7-…`, `claude-…-6` — keeps
 * resolving without a catalogue change. `haider` is not a brand: the daemon's
 * own provider id must not claim somebody's mark.
 *
 * Kept free of Compose so the resolution can be unit-tested directly, exactly
 * as the JS split does (owner addition H5).
 */
data class BrandFamily(
    val key: String,
    val label: String,
    val color: Color,
    /** Light-theme colour where the dark one would vanish (white marks). */
    val colorLight: Color = color,
    /** Letter tile for brands we have no real mark for. */
    val letter: String? = null,
    val model: List<String>,
    val provider: List<String>,
)

val BRAND_FAMILIES = listOf(
    BrandFamily(
        key = "openai",
        label = "OpenAI",
        color = Color(0xFFFFFFFF),
        colorLight = Color(0xFF0D0D0D),
        model = listOf("gpt", "openai", "codex", "sol", "o1", "o3", "o4"),
        provider = listOf("openai"),
    ),
    BrandFamily(
        key = "claude",
        label = "Claude",
        color = Color(0xFFD97757),
        model = listOf("claude", "anthropic", "fable", "opus", "sonnet", "haiku", "mythos"),
        provider = listOf("anthropic"),
    ),
    BrandFamily(
        key = "deepseek",
        label = "DeepSeek",
        color = Color(0xFF4D6BFE),
        model = listOf("deepseek"),
        provider = listOf("deepseek"),
    ),
    BrandFamily(
        key = "gemini",
        label = "Gemini",
        color = Color(0xFF4E86F5),
        model = listOf("gemini", "antigravity", "google", "palm"),
        provider = listOf("google", "gemini"),
    ),
    BrandFamily(
        key = "grok",
        label = "Grok",
        color = Color(0xFFFFFFFF),
        colorLight = Color(0xFF0D0D0D),
        model = listOf("grok", "xai"),
        provider = listOf("xai", "grok"),
    ),
    BrandFamily(
        key = "qwen",
        label = "Qwen",
        color = Color(0xFF615CED),
        letter = "Q",
        model = listOf("qwen", "qwq"),
        provider = listOf("alibaba", "qwen"),
    ),
    BrandFamily(
        key = "kimi",
        label = "Kimi",
        color = Color(0xFF16A8F0),
        letter = "K",
        model = listOf("kimi", "moonshot"),
        provider = listOf("moonshot", "kimi"),
    ),
    BrandFamily(
        key = "mistral",
        label = "Mistral",
        color = Color(0xFFFF7000),
        letter = "M",
        model = listOf("mistral", "magistral", "devstral", "codestral"),
        provider = listOf("mistral"),
    ),
    BrandFamily(
        key = "meta",
        label = "Llama",
        color = Color(0xFF0668E1),
        letter = "L",
        model = listOf("llama", "meta"),
        provider = listOf("meta"),
    ),
    BrandFamily(
        key = "glm",
        label = "GLM",
        color = Color(0xFF3859FF),
        letter = "G",
        model = listOf("glm", "zhipu"),
        provider = listOf("zhipu", "zai"),
    ),
)

/** Model id wins; the provider is only consulted when the model says nothing. */
fun modelBrandFor(model: String?, provider: String?): BrandFamily? {
    val modelText = model.orEmpty().lowercase()
    val providerText = provider.orEmpty().lowercase()
    if (modelText.isNotEmpty()) {
        BRAND_FAMILIES.firstOrNull { family ->
            family.model.any { modelText.contains(it) }
        }?.let { return it }
    }
    if (providerText.isNotEmpty() && providerText != "haider") {
        BRAND_FAMILIES.firstOrNull { family ->
            family.provider.any { providerText.contains(it) }
        }?.let { return it }
    }
    return null
}
