-- D1b: aq-git §10.3 reconcile/resolve run under their own frozen operation IDs.
-- Their intents and receipts are the immutable audit of how a hold was released.
ALTER TABLE delivery_intents DROP CONSTRAINT delivery_intents_operation_check;
ALTER TABLE delivery_intents ADD CONSTRAINT delivery_intents_operation_check
 CHECK (operation IN ('candidate.create','candidate.show','publish','operation.show','operation.reconcile','operation.resolve'));
UPDATE meta SET value='77' WHERE key='state_format_version';
