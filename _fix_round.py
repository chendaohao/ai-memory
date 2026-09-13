import io

# 1. install_mcp.rs: unused clap import + missing unwrap on opencode2 builder
p = 'crates/ai-memory-cli/src/commands/install_mcp.rs'
s = io.open(p, encoding='utf-8').read()
s = s.replace('''mod tests {
    use super::*;
    use clap::Parser;
    use std::fs;
    use tempfile;''', '''mod tests {
    use super::*;
    use std::fs;
    use tempfile;''')
old = '''            (
                "opencode2",
                build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist"),
            ),'''
new = '''            (
                "opencode2",
                build_opencode2_plugin("http://127.0.0.1:49374", Some("tok"), None, "denylist")
                    .unwrap(),
            ),'''
assert old in s, 'opencode2 unwrap'
s = s.replace(old, new, 1)
io.open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('install_mcp fixed')

# 2. install_skills.rs: two stale 7-arg test calls -> 5-arg
p = 'crates/ai-memory-cli/src/commands/install_skills.rs'
s = io.open(p, encoding='utf-8').read()
old = '''            Path::new("/repo"),
            Some(Path::new("/home/alice")),
            None,
            None,
            Some(Path::new("/stores/claude")),
            SkillHostPlatform::Other,
        )'''
new = '''            Path::new("/repo"),
            Some(Path::new("/home/alice")),
            Some(Path::new("/stores/claude")),
            SkillHostPlatform::Other,
        )'''
assert s.count(old) == 2, s.count(old)
s = s.replace(old, new)
io.open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('install_skills fixed')

# 3. hook.rs: collapse the double blank left by the kiro test removal
p = 'crates/ai-memory-cli/src/commands/hook.rs'
s = io.open(p, encoding='utf-8').read()
old = '''        assert!(report.contains("0 DROPPED"), "{report}");
    }


    #[tokio::test]'''
new = '''        assert!(report.contains("0 DROPPED"), "{report}");
    }

    #[tokio::test]'''
assert old in s, 'hook blank'
s = s.replace(old, new, 1)
io.open(p, 'w', encoding='utf-8', newline='\n').write(s)
print('hook blank fixed')
