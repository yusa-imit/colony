# 01. Posting to chat marks unread messages as read

Kind: bug · Area: `board`

## Problem

`Board::post` moved the poster's read cursor to the new message's sequence number ("an agent's own message
counts as read"). Every message from other workers that arrived since the poster last read chat was marked read
along with it, and never showed up in `chat_read`, `board_view` or a nudge again.

## Evidence

Replaying `tools.jsonl` against the cursor rules: **29 deliveries were skipped** by this jump.

The costly one: at 21.7 min w1 posted `@all` "I will take T22 (README wrap-up); drafting it locally now" (#10).
At 22.3 min w2 posted #12, which marked #10 and #11 as read. w2's next `board_view` (22.8 min) showed no chat
line at all, and at 23.2 min w2 claimed T22. w1's README draft (one turn, $1.07) was thrown away, and three other
workers' messages asking w1 to take T22 (#13–#15) were moot.

The unit test `chat_cursor_and_mentions` only covered posting with nothing unread.

## Fix

- `post` no longer touches the cursor.
- `unread` already skipped the agent's own messages; `read_chat` now skips them too, so a worker never reads its
  own posts back.

## Verification

- `board::tests::posting_keeps_unread_messages_from_others`: w2 has one unread message, posts, and still has
  exactly that message unread; `read_chat` returns it and not w2's own post.
