# Template placeholders

Reply attributions (Settings > Accounts > Reply attribution), the forward header and snippets
(Message > Insert Snippet…, Window > Snippets) are written as format strings. A placeholder is `${name}`, `${name:argument}` or `${name|fallback}`; `$${` writes a
literal `${`. Date arguments are Unicode (ICU) patterns, for example `${date:d/M/yyyy}`.
Unknown placeholders are left as typed and flagged in the editor.

This table is checked against `TemplateFormatter.placeholders` by `TemplateFormatterTests`.

| Placeholder | Meaning | Example |
|---|---|---|
| `${sender}` | Sender's name, else their address | Ada Lovelace |
| `${sender.name}` | Sender's display name | Ada Lovelace |
| `${sender.first}` | First word of the sender's name | Ada |
| `${sender.email}` | Sender's address | ada@example.org |
| `${date}` | Date of the original message | 16 Oct 2026, 09:30 |
| `${date:PATTERN}` | The same with a Unicode date pattern, e.g. ${date:d/M/yyyy} | 16/10/2026 |
| `${subject}` | Subject of the original (or the Subject field) | Lunch? |
| `${recipient}` | First To recipient of the message being written | Grace Hopper |
| `${recipient.name}` | Their display name | Grace Hopper |
| `${recipient.first}` | First word of their name | Grace |
| `${recipient.email}` | Their address | grace@example.org |
| `${my.name}` | Your name on this account | Marc |
| `${my.email}` | Your address on this account | marc@example.org |
| `${today}` | Today's date and time | 16 Oct 2026, 09:41 |
| `${today:PATTERN}` | Today with a Unicode date pattern | 2026-10-16 |
| `${cursor}` | Where the caret lands after inserting a snippet |  |
