# Testing a client

How to check a mail app against Tolk, for example one not tested yet (see
[compatibility.md](compatibility.md)). Use test accounts, not your real
mailbox. `scripts/run-scenarios.sh` covers the server side automatically;
this list is about what the app shows.

| Step | Expected |
|---|---|
| Add the account with only address and password | Autodiscover finds the server; folders, contacts and calendar appear. |
| Initial mail sync | Recent mail with correct senders, dates, read state and attachments. |
| Push | A message sent to the account shows up within seconds while the app is in the background. |
| Read, unread, flag | Shows in another client (webmail) within seconds, and the other way round. |
| Delete, move | The message moves to Trash or the target folder everywhere. |
| Send with an attachment | Arrives with the attachment; a copy is in Sent. |
| Reply and forward | Quote and attachments are right; the original shows as answered or forwarded. |
| Search | Finds older mail not on the phone; recipient suggestions while typing. |
| Folders | Creating, renaming and deleting a folder on the phone shows elsewhere, and the other way round. |
| Contacts | A contact created, changed and deleted on the phone matches the server, and the other way round. |
| Calendar | An event, a recurring event, moving and deleting one occurrence, all both ways. |
| Invitations | An invitation from another account shows with its accept/decline buttons; the answer reaches the organizer. |
| Out of office | Turning it on and off on the phone changes the server's auto-reply. |
| Drafts (EAS 16 clients) | A draft saved on the phone appears in Drafts on the server and can be sent. |

Note the app and its version, and the EAS version it chose (`tolk devices
list`), with the result of each step. Reports for untested apps are
welcome as GitHub issues; for a step that fails, the device's traffic
(see operations.md, traffic capture) helps most, once checked for private
data.
