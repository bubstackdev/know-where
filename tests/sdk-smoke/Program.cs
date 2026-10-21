using System.Text.Json;
using Anthropic;
using Anthropic.Exceptions;
using Anthropic.Models.Messages;

var client = new AnthropicClient { BaseUrl = args[0], ApiKey = args[1], MaxRetries = 0 };
var raw = JsonSerializer.Deserialize<Dictionary<string, JsonElement>>(JsonSerializer.Serialize(new
{
    model = "high", max_tokens = 1024,
    messages = new[] { new { role = "user", content = args[2] } },
}))!;
var request = MessageCreateParams.FromRawUnchecked(
    new Dictionary<string, JsonElement>(), new Dictionary<string, JsonElement>(), raw);

try
{
    var message = await client.Messages.CreateStreaming(request).Aggregate();
    if (args[2] == "ERROR") throw new Exception("Expected SDK SSE exception");
    var thinking = message.Content.Select(b => b.TryPickThinking(out var t) ? t : null)
        .FirstOrDefault(t => t is not null);
    var tools = message.Content.Select(b => b.TryPickToolUse(out var t) ? t : null)
        .Where(t => t is not null).ToArray();
    var text = string.Concat(message.Content.Select(b => b.TryPickText(out var t) ? t.Text : ""));
    if (thinking?.Thinking != "Consider carefully" || thinking.Signature != "local:")
        throw new Exception("Thinking block mismatch");
    if (text != "Hello world" || tools.Length != 2 || tools.Any(t => t!.Name != "lookup"))
        throw new Exception("Text/tool aggregation mismatch");
    if (JsonSerializer.Serialize(tools[0]!.Input) != "{\"q\":\"rust\"}"
        || JsonSerializer.Serialize(tools[1]!.Input) != "{\"q\":\"second\"}")
        throw new Exception("Fragmented tool arguments mismatch");
    if (message.StopReason!.Raw() != "tool_use"
        || message.Usage.InputTokens != 11 || message.Usage.OutputTokens != 5)
        throw new Exception("Completion/usage mismatch");
    Console.WriteLine("PASS official Anthropic SDK: reasoning, text, parallel fragmented tools and usage");
}
catch (AnthropicSseException e) when (args[2] == "ERROR")
{
    if (!e.Message.Contains("out of memory")) throw;
    Console.WriteLine("PASS official Anthropic SDK: committed failure raises AnthropicSseException");
}
