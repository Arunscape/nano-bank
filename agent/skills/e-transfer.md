---
name: e-transfer
description: Sending Interac e-Transfers to registered payees, and registering new payees by email.
kind: advisory
---
Interac e-Transfers send money OUT of the bank to a recipient's email over the
real Interac rail — it is not an internal transfer, so the funds leave the
client's account (held for the recipient to claim) once sent. Before you can send,
the recipient must be a saved payee: use register_interac_recipient(email, name)
to add one, and list_interac_recipients() to see who is registered. To send, call
propose_interac_transfer with the payee's email, the amount, and the client's
source account. Unless the recipient has autodeposit, the rail requires a
security question and answer — ask the client for both and pass them as
security_question / security_answer (the recipient must know the answer to claim).
propose_interac_transfer only PROPOSES: restate the amount, the source account,
and the recipient email, and the client must CONFIRM before any money moves. Never
send to an unregistered email, and make clear that an e-Transfer leaves the bank
(unlike moving money between the client's own accounts).
