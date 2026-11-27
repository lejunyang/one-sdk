// npm may roll back its package tree before returning the install error.
// Record successful symlink writes while their inode is still available.
const fs = require('fs');
const path = require('path');

if (String(process.ppid) === process.env.OSDK_NPM_MAN_PARENT) {
    const journal = process.env.OSDK_NPM_MAN_JOURNAL;
    const prefix = path.resolve(process.env.OSDK_NPM_MAN_PREFIX);
    const originalOptions = process.env.OSDK_NPM_ORIGINAL_NODE_OPTIONS;
    delete process.env.OSDK_NPM_MAN_JOURNAL;
    delete process.env.OSDK_NPM_MAN_PREFIX;
    delete process.env.OSDK_NPM_MAN_PARENT;
    delete process.env.OSDK_NPM_ORIGINAL_NODE_OPTIONS;
    if (originalOptions === undefined) {
        delete process.env.NODE_OPTIONS;
    } else {
        process.env.NODE_OPTIONS = originalOptions;
    }

    const manRoots = new Set([prefix, fs.realpathSync(prefix)].map(p => path.join(p, 'share/man')));
    const promises = fs.promises;
    if (promises) {
        const symlink = promises.symlink;
        promises.symlink = async function (target, to, type) {
            if (typeof to !== 'string' || typeof target !== 'string') {
                return symlink(target, to, type);
            }
            const section = path.dirname(to);
            if (!manRoots.has(path.dirname(section)) || !/^man[0-9]+$/.test(path.basename(section))) {
                return symlink(target, to, type);
            }

            const directory = fs.realpathSync(section);
            await symlink(target, to, type);
            const entry = path.join(directory, path.basename(to));
            try {
                if (fs.realpathSync(section) !== directory || fs.readlinkSync(entry) !== target) {
                    return;
                }
                const stat = fs.lstatSync(entry, { bigint: true });
                if (stat.isSymbolicLink() && stat.ctimeNs !== undefined) {
                    const identity = [stat.dev, stat.ino, stat.ctimeNs].join(':');
                    fs.appendFileSync(journal, JSON.stringify({ path: entry, target, identity }) + '\n');
                }
            } catch (error) {
                // Another writer may have removed the link or replaced it with a file.
                if (error.code !== 'ENOENT' && error.code !== 'EINVAL') {
                    throw error;
                }
            }
        };
    }
}
