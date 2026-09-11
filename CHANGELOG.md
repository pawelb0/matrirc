# Changelog

## 0.3.0 (2026-05-22)

After upgrading, run `matrirc install-irssi --force` to update the irssi
script. The new script displays the message IDs used for replies.

### Added

- Reply to a message with `!r <id> text`. Incoming messages have three-letter
  IDs; replies reference the original Matrix event and include a text fallback.
- Join rooms by alias or ID with
  `/msg matrirc join <#alias:server or !room:server>`.
- Request admission to a room with `/msg matrirc knock <target> [reason]`.
- Control reply-ID display per connection with
  `/msg matrirc ids on|off|toggle|status`. The `show_reply_ids` setting in
  `config.toml` sets the default.
- Inspect stored reply targets with `/msg matrirc dump <window>`.
