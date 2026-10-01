//! Who may do what in a guild. Shared by the Discord commands and the web UI, so both apply the
//! same rules; callers gather the facts (from an interaction payload or from Discord REST).

use serenity::all::Permissions;

/// Most roles of one kind a guild may configure.
pub const MAX_ROLES_PER_KIND: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleKind {
    /// May use /talk and the web chat.
    Use,
    /// May manage the guild's knowledge base.
    Manage,
}

impl RoleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Manage => "manage",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "use" => Some(Self::Use),
            "manage" => Some(Self::Manage),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Use => "利用",
            Self::Manage => "ナレッジ管理",
        }
    }
}

/// A guild's settings as stored in the database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuildAccess {
    /// On the operator's allowlist (and not denied).
    pub allowed: bool,
    pub use_roles: Vec<u64>,
    pub manage_roles: Vec<u64>,
}

impl GuildAccess {
    pub fn roles(&self, kind: RoleKind) -> &[u64] {
        match kind {
            RoleKind::Use => &self.use_roles,
            RoleKind::Manage => &self.manage_roles,
        }
    }
}

/// The member whose access is being decided.
pub struct Member<'a> {
    pub guild_id: u64,
    /// Role IDs as Discord reports them; @everyone is never included.
    pub roles: &'a [u64],
    /// Guild-level permissions (Discord resolves owner and ADMINISTRATOR to all permissions).
    pub permissions: Permissions,
    pub is_owner: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Access {
    /// May use /talk and the web chat.
    pub use_bot: bool,
    /// May upload and delete knowledge base documents.
    pub manage_knowledge: bool,
    /// May change the guild's role settings.
    pub configure: bool,
}

/// Nothing is granted outside allowlisted guilds. Using the bot always requires a configured
/// role, administrators included; configuring requires owner, ADMINISTRATOR or MANAGE_GUILD,
/// so a knowledge-manager role cannot grant itself more.
pub fn decide(guild: &GuildAccess, member: &Member) -> Access {
    if !guild.allowed {
        return Access::default();
    }
    let holds = |roles: &[u64]| {
        roles
            .iter()
            .any(|role| *role == member.guild_id || member.roles.contains(role))
    };
    let configure = member.is_owner
        || member
            .permissions
            .intersects(Permissions::ADMINISTRATOR | Permissions::MANAGE_GUILD);
    Access {
        use_bot: holds(&guild.use_roles),
        manage_knowledge: configure || holds(&guild.manage_roles),
        configure,
    }
}

/// Displays a role without pinging it; @everyone has the guild's ID.
pub fn role_mention(guild_id: u64, role_id: u64) -> String {
    if role_id == guild_id {
        "@everyone".into()
    } else {
        format!("<@&{role_id}>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 1000;

    fn guild(allowed: bool, use_roles: &[u64], manage_roles: &[u64]) -> GuildAccess {
        GuildAccess {
            allowed,
            use_roles: use_roles.to_vec(),
            manage_roles: manage_roles.to_vec(),
        }
    }

    fn decide_for(guild: &GuildAccess, roles: &[u64], permissions: Permissions) -> Access {
        decide(
            guild,
            &Member {
                guild_id: GUILD,
                roles,
                permissions,
                is_owner: false,
            },
        )
    }

    #[test]
    fn nothing_is_granted_outside_allowlisted_guilds() {
        let denied = guild(false, &[1], &[2]);
        assert_eq!(
            decide_for(&denied, &[1, 2], Permissions::all()),
            Access::default()
        );
        let owner = Member {
            guild_id: GUILD,
            roles: &[],
            permissions: Permissions::empty(),
            is_owner: true,
        };
        assert_eq!(decide(&denied, &owner), Access::default());
    }

    #[test]
    fn use_requires_a_configured_role_without_admin_bypass() {
        let none = guild(true, &[], &[]);
        assert!(!decide_for(&none, &[1], Permissions::empty()).use_bot);
        let admin = decide_for(&none, &[], Permissions::ADMINISTRATOR);
        assert!(!admin.use_bot);
        assert!(admin.configure && admin.manage_knowledge);

        let configured = guild(true, &[1, 3], &[]);
        assert!(decide_for(&configured, &[3], Permissions::empty()).use_bot);
        assert!(!decide_for(&configured, &[2], Permissions::empty()).use_bot);
        assert!(!decide_for(&configured, &[], Permissions::ADMINISTRATOR).use_bot);
    }

    #[test]
    fn everyone_role_grants_every_member() {
        let everyone = guild(true, &[GUILD], &[]);
        assert!(decide_for(&everyone, &[], Permissions::empty()).use_bot);
        assert!(!decide_for(&everyone, &[], Permissions::empty()).manage_knowledge);
    }

    #[test]
    fn knowledge_managers_cannot_configure() {
        let settings = guild(true, &[1], &[2]);
        let manager = decide_for(&settings, &[2], Permissions::empty());
        assert_eq!(
            manager,
            Access {
                use_bot: false,
                manage_knowledge: true,
                configure: false,
            }
        );
        let manage_guild = decide_for(&settings, &[], Permissions::MANAGE_GUILD);
        assert!(manage_guild.configure && manage_guild.manage_knowledge);
        assert!(!manage_guild.use_bot);
        let owner = decide(
            &settings,
            &Member {
                guild_id: GUILD,
                roles: &[1],
                permissions: Permissions::empty(),
                is_owner: true,
            },
        );
        assert_eq!(
            owner,
            Access {
                use_bot: true,
                manage_knowledge: true,
                configure: true,
            }
        );
    }

    #[test]
    fn kinds_and_mentions() {
        for kind in [RoleKind::Use, RoleKind::Manage] {
            assert_eq!(RoleKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(RoleKind::parse("admin"), None);
        assert_eq!(role_mention(GUILD, GUILD), "@everyone");
        assert_eq!(role_mention(GUILD, 5), "<@&5>");
    }
}
