-- Where we stand in the group (as of `group_revision`): 0 neither invited nor asking (a member,
-- or not one any more), 1 invited and not answered, 2 asked to join through its link.
ALTER TABLE conversations RENAME COLUMN group_invited TO group_standing;
