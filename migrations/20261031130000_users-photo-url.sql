-- A user's own picture (My Account > Picture, and the phone app's profile): a
-- file in the `user-photos` folder, shown wherever the user's avatar is.
ALTER TABLE users ADD COLUMN photo_url VARCHAR(1024) NULL;
