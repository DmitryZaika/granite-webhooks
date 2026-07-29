-- Step 1: Create the email_participants table
CREATE TABLE email_participants (
    id INT AUTO_INCREMENT PRIMARY KEY,
    email_id INT NOT NULL,
    email VARCHAR(255) NOT NULL,
    display_name VARCHAR(255) NULL,
    user_id INT NULL,
    type ENUM('from', 'to', 'cc', 'bcc') NOT NULL,
    created_date TIMESTAMP DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT fk_email_participants_email
        FOREIGN KEY (email_id) REFERENCES emails(id)
        ON DELETE CASCADE,

    CONSTRAINT fk_email_participants_user
        FOREIGN KEY (user_id) REFERENCES users(id)
        ON DELETE SET NULL
);

CREATE INDEX idx_email_participants_email_id ON email_participants(email_id);
CREATE INDEX idx_email_participants_user_id ON email_participants(user_id);

-- Step 2: Migrate existing sender data as 'from' type
INSERT INTO email_participants (email_id, email, user_id, type)
SELECT id, sender_email, sender_user_id, 'from'
FROM emails
WHERE sender_email IS NOT NULL;

-- Step 2: Migrate existing receiver data as 'to' type
INSERT INTO email_participants (email_id, email, user_id, type)
SELECT id, receiver_email, receiver_user_id, 'to'
FROM emails
WHERE receiver_email IS NOT NULL;

-- Step 3: Drop foreign keys linking to the old columns
ALTER TABLE emails DROP FOREIGN KEY fk_sender_user;
ALTER TABLE emails DROP FOREIGN KEY fk_receiver_user;

-- Step 3: Remove the old columns from the emails table
ALTER TABLE emails
    DROP COLUMN sender_user_id,
    DROP COLUMN receiver_user_id,
    DROP COLUMN sender_email,
    DROP COLUMN receiver_email;
