const fs = require('fs');
const path = require('path');

const root = path.join(__dirname, '..');
const { version } = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));

const content = `// Generated from package.json by builder/gen-version.js; a macro so \`concat!\` can NUL-terminate it.
macro_rules! plugins_version {
    () => {
        "${version}"
    };
}
`;

const out = path.join(root, 'version.rs');
fs.writeFileSync(out, content, 'utf8');
console.log(`Generated version.rs -> v${version}`);
