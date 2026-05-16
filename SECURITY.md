# Security Policy

## Reporting a Vulnerability

We take the security of Runtimo seriously. If you discover a security vulnerability, please report it privately.

**Please do NOT open public issues or pull requests for security vulnerabilities.**

### How to Report

1. **Email**: Send details to [security@runtimo.dev](mailto:security@runtimo.dev) (replace with your email)
2. **Include**:
   - Description of the vulnerability
   - Steps to reproduce
   - Potential impact
   - Suggested fix (if any)

### What to Expect

- **Initial response**: Within 48 hours
- **Status updates**: Weekly while we investigate
- **Timeline**: We aim to resolve critical issues within 7 days
- **Disclosure**: Coordinated disclosure after fix is available

### Scope

Security vulnerabilities include:
- Path traversal attacks
- Unauthorized file access
- Process injection
- Resource exhaustion vulnerabilities
- Data leakage through WAL or backups

### Out of Scope

The following are generally not considered security vulnerabilities:
- Missing compiler warnings
- Non-security-related bugs
- Feature requests

## Security Best Practices

### For Users

1. **Keep updated**: Always use the latest version
2. **Review WAL logs**: Monitor for unusual activity
3. **Set resource limits**: Use system-level controls
4. **Audit capabilities**: Review what each capability can access

### For Contributors

1. **No secrets**: Never commit API keys, tokens, or credentials
2. **Validate inputs**: All user inputs must be validated
3. **Path safety**: Use canonical paths, reject `..` sequences
4. **Resource guards**: Check CPU/RAM limits before execution

## Version Support

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |
| < 0.1   | :x:                |

## Acknowledgments

We appreciate responsible disclosure and will acknowledge reporters (with permission) in our security advisories.

---

For general questions, please open an issue on GitHub.
