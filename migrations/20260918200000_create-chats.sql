-- Persisted AI Chat conversations. Each chat belongs to one user; every
-- visible message is its own row so history can be listed and reopened.
CREATE TABLE IF NOT EXISTS chats (
  id INT AUTO_INCREMENT PRIMARY KEY,
  user_id INT NOT NULL,
  company_id INT NOT NULL,
  title VARCHAR(200) NOT NULL DEFAULT 'New chat',
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  INDEX idx_chats_user_updated (user_id, updated_at),
  CONSTRAINT fk_chats_user
    FOREIGN KEY (user_id)
    REFERENCES users(id)
    ON DELETE CASCADE,
  CONSTRAINT fk_chats_company
    FOREIGN KEY (company_id)
    REFERENCES company(id)
    ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

-- sender is 'user' or 'ai'. is_agentic is reserved and left NULL for now.
CREATE TABLE IF NOT EXISTS chat_messages (
  id INT AUTO_INCREMENT PRIMARY KEY,
  chat_id INT NOT NULL,
  sender ENUM('user', 'ai') NOT NULL,
  content TEXT NOT NULL,
  is_agentic TINYINT(1) NULL DEFAULT NULL,
  payload JSON NULL,
  created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
  INDEX idx_chat_messages_chat_created (chat_id, created_at),
  CONSTRAINT fk_chat_messages_chat
    FOREIGN KEY (chat_id)
    REFERENCES chats(id)
    ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
