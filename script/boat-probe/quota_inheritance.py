"""Job-side check of the Boat quota-inheritance ioctl boundary."""

SOURCE = r'''
def verify_quota_inheritance(directory):
    import ctypes, errno, fcntl, os, struct
    child = directory / 'quota-inheritance-child'
    child.mkdir()
    fd = os.open(child, os.O_RDONLY | os.O_DIRECTORY)
    def attributes(descriptor):
        data = bytearray(28)
        fcntl.ioctl(descriptor, 0x801c581f, data, True)
        return data, struct.unpack_from('=IIII', data)
    try:
        original, (flags, _, _, project) = attributes(fd)
        assert project != 0 and flags & 0x200, 'directory is not project-inheriting'
        changed = bytearray(original)
        struct.pack_into('=I', changed, 0, flags & ~0x200)
        refused = []
        for request, data in ((0x401c5820, changed),
                              (0x40086602, bytearray(8)),
                              (0x40046602, bytearray(4))):
            try:
                fcntl.ioctl(fd, request, data)
            except OSError as error:
                assert error.errno == errno.EPERM, (request, error.errno)
                refused.append(request)
            else:
                raise AssertionError('job cleared quota inheritance')
        libc = ctypes.CDLL(None, use_errno=True)
        libc.syscall.restype = ctypes.c_long
        _, extent_size, extents, _, cow_extent_size = struct.unpack_from('=IIIII', original)
        data = ctypes.create_string_buffer(struct.pack('=QIIII', flags & ~0x200, extent_size, extents, project, cow_extent_size))
        result = libc.syscall(ctypes.c_long(469), ctypes.c_int(fd), ctypes.c_char_p(b''),
                              ctypes.byref(data), ctypes.c_size_t(24), ctypes.c_uint(0x1000))
        setter_errno = ctypes.get_errno() if result == -1 else 0
        assert setter_errno == errno.ENOSYS, ('file_setattr', result, setter_errno)
        after, values = attributes(fd)
        assert after == original, 'attribute setter changed directory state'
        # This getter must remain usable even though its corresponding setter is denied.
        fcntl.ioctl(fd, 0x80086601, bytearray(8), True)
        path = child / 'inherited-file'
        with path.open('w+b') as output:
            output.write(b'x')
            output.flush()
            _, file_attributes = attributes(output.fileno())
        path.unlink()
        assert file_attributes[3] == project, 'new file escaped the job project'
        return child, dict(project=project, child_project=file_attributes[3], setters_refused=refused,
                           file_setattr_errno=setter_errno)
    finally:
        os.close(fd)
'''

# Run by the operator as root, outside the job's syscall filter, on the same XFS
# mount. Successful FSSETXATTR transitions show that this ioctl works on XFS,
# so its job-side refusal is not due to missing filesystem support.
POSITIVE_CONTROL = r'''
import ctypes, errno, fcntl, json, os, pathlib, struct, subprocess, tempfile
if os.uname().machine != 'x86_64':
    raise SystemExit('attribute syscall control requires the x86-64 Boat image')
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
with tempfile.TemporaryDirectory(prefix='quota-attribute-control-', dir='/var/lib/motor-boat') as directory:
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    def attributes():
        data = bytearray(28)
        fcntl.ioctl(fd, 0x801c581f, data, True)
        return data, struct.unpack_from('=IIIII', data)
    original, (flags, extent_size, extents, project, cow_extent_size) = attributes()
    try:
        inherited = bytearray(original)
        struct.pack_into('=I', inherited, 0, flags | 0x200)
        fcntl.ioctl(fd, 0x401c5820, inherited)
        _, before = attributes()
        cleared = bytearray(original)
        struct.pack_into('=I', cleared, 0, flags & ~0x200)
        fcntl.ioctl(fd, 0x401c5820, cleared)
        _, after = attributes()
        assert before[0] & 0x200 and not (after[0] & 0x200) and before[3] == after[3]
        # Exercise the path-based setter on supporting kernels, not merely its number.
        fcntl.ioctl(fd, 0x401c5820, inherited)
        data = ctypes.create_string_buffer(struct.pack('=QIIII', flags & ~0x200, extent_size, extents, project, cow_extent_size))
        result = libc.syscall(ctypes.c_long(469), ctypes.c_int(fd), ctypes.c_char_p(b''),
                              ctypes.byref(data), ctypes.c_size_t(24), ctypes.c_uint(0x1000))
        setter_errno = ctypes.get_errno() if result == -1 else 0
        assert setter_errno in (0, errno.ENOSYS), ('unfiltered file_setattr', result, setter_errno)
        if setter_errno == 0:
            _, file_after = attributes()
            assert not (file_after[0] & 0x200) and file_after[3] == project
        report = dict(request=0x401c5820, clear_succeeded=True, before_flags=before[0], after_flags=after[0],
                      before_project=before[3], after_project=after[3], file_setattr_supported=setter_errno == 0,
                      file_setattr_errno=setter_errno, kernel_release=subprocess.check_output(['uname', '-r'], text=True).strip())
    finally:
        fcntl.ioctl(fd, 0x401c5820, original)
        os.close(fd)
print(json.dumps(report))
'''
