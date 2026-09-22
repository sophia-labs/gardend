use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoopbackScopeDetail {
    scope: &'static str,
    category: &'static str,
    access: &'static str,
    description: &'static str,
    default_grant: bool,
}

impl LoopbackScopeDetail {
    pub(crate) const fn key(&self) -> &'static str {
        self.scope
    }

    pub(crate) const fn access(&self) -> &'static str {
        self.access
    }

    #[cfg(test)]
    pub(crate) const fn default_grant(&self) -> bool {
        self.default_grant
    }

    pub(crate) fn with_default_grant(self, default_grant: bool) -> LoopbackScopeDetail {
        LoopbackScopeDetail {
            default_grant,
            ..self
        }
    }
}

pub(crate) const fn scope(
    scope: &'static str,
    category: &'static str,
    access: &'static str,
    description: &'static str,
) -> LoopbackScopeDetail {
    LoopbackScopeDetail {
        scope,
        category,
        access,
        description,
        default_grant: true,
    }
}
