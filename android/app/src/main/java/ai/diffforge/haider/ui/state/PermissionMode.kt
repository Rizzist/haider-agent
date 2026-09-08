package ai.diffforge.haider.ui.state

/**
 * How much the model may do without asking (owner addition H6).
 *
 * The owner's instruction was "model work should be automated (like reading
 * SMS etc..) not requiring permission". [Auto] is the standing consent given
 * once, on the first-run autonomy step: device-capability tool calls run
 * without an approval card. It is not blanket consent — a genuine question, a
 * secret, or a provider refusal still stops and asks.
 *
 * The daemon owns the policy; this is the UI's view of it and the door it sets
 * it through.
 */
enum class PermissionMode { Auto, Ask }
