"""Job-side check of the Boat quota-inheritance ioctl boundary."""

SOURCE = r'''
def verify_quota_inheritance(directory):
    import errno, fcntl, os, struct
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
        return child, dict(project=project, child_project=file_attributes[3], setters_refused=refused)
    finally:
        os.close(fd)
'''
