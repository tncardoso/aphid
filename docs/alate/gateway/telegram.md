# Telegram

A Telegram bot can speak to the alate. You send a message, the agent answers,
and you can permit or refuse a tool from the chat.

The bot is a client of the [gateway](../gateway.md), and not a second door. Each
chat attaches to the same socket and gets its own conversation, in the same
manner as a terminal. So two chats do not see each other, and `aphid alate
attach` shows what a chat said and what the agent answered.

This is behind a build feature, because it adds an HTTP client that a build
without a bot does not need:

```console
$ cargo build --release --features telegram
```

## Make a bot

1. Speak to `@BotFather` in Telegram and send `/newbot`. It gives you a token.
2. Put the token in the environment of the daemon:
   ```console
   $ export TELEGRAM_BOT_TOKEN=123456:AA...
   ```
3. Put a `telegram` block in `alate.json`:
   ```json
   { "gateway": { "telegram": { "chats": [], "tools": true } } }
   ```
4. Start the alate, and send a message to the bot. The bot refuses, and the
   refusal holds the id of your chat.
5. Put that id in `chats`, and start the alate again.

| Field | Effect |
| --- | --- |
| `token_env` | The variable that holds the bot token. `TELEGRAM_BOT_TOKEN` when absent. |
| `chats` | The chats that can speak to this alate, by id. An empty list permits nobody. |
| `poll` | How long one request waits for a message: `25s` when absent. |
| `tools` | Show one line for each tool call. `false` when absent. |
| `api` | The address of the Bot API. The Telegram one when absent. |

The token is never in `alate.json`, only the name of the variable that holds it.
This is the rule the model keys follow, and for the same cause: a configuration
file is copied and shared, and a token in it goes with it.

`chats` is an allow list, and an empty one permits nobody. Anything that can
speak to the bot can make the agent run commands, so a bot that anybody found
would be a bot that anybody could use. A chat that is refused is told its id one
time.

## In a chat

| What you send | Effect |
| --- | --- |
| Anything else | Words for the agent. |
| `/new` | Start a new conversation. The one before it stays on disk. |
| `/cancel` | Stop the run in flight. |
| `/start`, `/help` | Show these commands. |

The agent's answer comes in one message for each turn, and not one for each
word. Telegram permits approximately one message each second for a chat, and a
message for each part of an answer would be held back. A long answer is cut into
messages of 4096 characters, at a line end where there is one.

The chat shows the text of the answer, and the errors. It does not show the
thinking, the tool arguments or the tool results. Use `aphid alate attach` to
read those. With `tools` set to `true`, each tool call also gives one short
line, which makes a long run legible from a telephone.

In `/sessions`, a chat is listed as `telegram: <chat id>` and not as `attached`,
so you can tell a conversation in a chat from one in a terminal.

## Permission from a chat

A permission question comes to the chat with three buttons: **Allow**, **Allow
always** and **Deny**. The question goes only to a chat with a run in flight. A
question that belongs to a terminal or to a job is left for the terminal to
answer.

Note that a chat that has spoken stays attached until the daemon stops. So an
alate with a bot **is attended**, and a tool that asks permission is asked in
the chat instead of being refused. Before a chat speaks for the first time, no
connection exists, and an unattended alate behaves as it does with no bot. Refer
to [Permissions](../../alate.md#permissions).

## When Telegram does not answer

If the bot cannot be reached, the daemon says so one time and tries again, and
waits longer after each failure up to one minute. It says so again when Telegram
answers.

The bot is not necessary for the alate to start. A token that is absent, a
`poll` that is not a length of time, and a Telegram that does not answer are all
reported and passed over.
